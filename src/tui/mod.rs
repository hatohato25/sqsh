use crate::completion::{CompletionCache, CompletionItem};
use ::skim::prelude::*;
use crossterm::{
    event::{self},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::borrow::Cow;
use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::task::JoinHandle;
use unicode_width::UnicodeWidthStr;

use crate::config::{BastionConfig, BastionSetting, Config};
use crate::connection::ConnectionManager;
use crate::error::{Error, Result};
use crate::i18n::TuiMsg;
use crate::query::QueryResult;
use crate::t;

mod input;
mod render;
mod skim;

/// skimでレコードを選択した際の返却アクション
pub(super) enum SkimAction {
    /// ドリルダウン: USE / SELECT FROM を実行する（show databases/tables 用）
    DrillDown(String),
    /// レコード選択: WHERE テンプレートとレコード詳細を返す（通常 SELECT 結果用）
    SelectRecord {
        where_template: String,
        record: SelectedRecord,
    },
}

/// 選択されたレコードの詳細情報（SQL入力画面でプレビュー表示用）
#[derive(Debug, Clone)]
pub(super) struct SelectedRecord {
    /// カラム名と値のペア
    columns: Vec<(String, String)>,
}

/// 単純な文字列をskimアイテムとして使うためのラッパー
///
/// String は SkimItem を直接実装していないため、このラッパーを使って
/// テーブル名・カラム名のリストをskimに渡す。
struct SimpleSkimItem(String);

impl SkimItem for SimpleSkimItem {
    fn text(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.0)
    }
}

/// skim結果表示用のアイテム
struct ResultRowItem {
    row_index: usize,
    display: String,
}

impl SkimItem for ResultRowItem {
    fn text(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.display)
    }

    fn output(&self) -> Cow<'_, str> {
        // previewコマンドの {} 置換で行インデックスのみを渡す
        // テキストを含めるとパイプ等の特殊文字がシェルパースエラーを起こすため
        Cow::Owned(self.row_index.to_string())
    }
}

/// 書き込み系SQLかどうかを判定する
///
/// readonlyモードでブロックすべきSQL文のプレフィックスをチェックする。
/// サーバー側でもブロックされるが、ユーザーへの即時フィードバックのためクライアントでも検査する。
/// `/* ... */` や `-- ...` で始まるコメントは読み飛ばして先頭の意味あるトークンを判定する。
pub(super) fn is_write_sql(sql: &str) -> bool {
    let first_token = first_meaningful_token(sql).to_uppercase();

    // WITH句（CTE）の場合、本体のDML部分を確認する
    if first_token == "WITH" {
        return cte_contains_write(sql);
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

/// CTE（WITH句）の本体部分が書き込みDMLかどうかを簡易判定する
///
/// WITH句の後に続くCTE定義（`name AS (...)`）を括弧のネストで追跡し、
/// 全CTE定義の終了後（depth==0 の `)` の後にカンマが続かない位置）の
/// 先頭トークンがDML書き込みキーワードかを確認する。
///
/// 完全なSQLパーサーではないため、極端にネストしたCTEでは誤判定の可能性があるが、
/// サーバー側のreadonly制約がバックアップとして機能する。
fn cte_contains_write(sql: &str) -> bool {
    let upper = sql.to_uppercase();
    let write_keywords = ["INSERT", "UPDATE", "DELETE"];

    // WITH句の後から走査を開始する
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
                // depth が 0 に戻った＝1つのCTE定義の括弧が閉じた
                if depth == 0 {
                    // カンマが続く場合はさらにCTE定義が続くのでスキップする
                    let remaining = upper[i..].trim_start();
                    if remaining.starts_with(',') {
                        // カンマの次のCTE定義へ進む
                        i += upper[i..].len() - remaining.len() + 1;
                        continue;
                    }
                    // カンマ以外が続く場合は全CTE定義が終わりDML本体に到達している
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

/// SQLの先頭コメント（`/* ... */` および `-- ...`）を読み飛ばし、最初の意味あるトークンを返す
///
/// コメントのみのSQLや閉じていないブロックコメントの場合は空文字列を返す。
fn first_meaningful_token(sql: &str) -> &str {
    let mut s = sql.trim();
    loop {
        if s.starts_with("/*") {
            // ブロックコメントを飛ばす
            if let Some(end) = s.find("*/") {
                s = s[end + 2..].trim_start();
            } else {
                // 閉じないブロックコメント: 残り全体がコメント扱い
                return "";
            }
        } else if s.starts_with("--") {
            // 行コメントを飛ばす
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

/// 文字列を表示幅ベースで固定幅にパディングする
///
/// 全角文字（2セル幅）を考慮し、ターミナル上で正しく列が揃うようにする。
/// 表示幅がtarget_widthを超える場合は切り詰めて"..."を付与する。
pub(super) fn pad_to_width(s: &str, target_width: usize) -> String {
    let display_width = UnicodeWidthStr::width(s);
    if display_width > target_width {
        // 表示幅ベースで切り詰め
        let mut truncated = String::new();
        let mut w = 0;
        let suffix = "...";
        let suffix_width = 3;
        let max_content_width = target_width.saturating_sub(suffix_width);
        for ch in s.chars() {
            let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if w + ch_width > max_content_width {
                break;
            }
            truncated.push(ch);
            w += ch_width;
        }
        truncated.push_str(suffix);
        // 切り詰め後の表示幅を再計算してパディング
        let truncated_width = UnicodeWidthStr::width(truncated.as_str());
        let padding = target_width.saturating_sub(truncated_width);
        format!("{}{}", truncated, " ".repeat(padding))
    } else {
        let padding = target_width.saturating_sub(display_width);
        format!("{}{}", s, " ".repeat(padding))
    }
}

/// データからカラムごとの最適な表示幅を計算する
///
/// カラム名と各行のデータの表示幅を比較し、最大幅を返す。
/// 最小幅4、最大幅40でクランプする。
pub(super) fn calculate_column_widths(columns: &[String], rows: &[Vec<String>]) -> Vec<usize> {
    let max_width = 40;
    let min_width = 4;

    let mut widths: Vec<usize> = columns
        .iter()
        .map(|col| UnicodeWidthStr::width(col.as_str()))
        .collect();

    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                let cell_width = UnicodeWidthStr::width(cell.as_str());
                if cell_width > widths[i] {
                    widths[i] = cell_width;
                }
            }
        }
    }

    widths
        .iter()
        .map(|&w| w.clamp(min_width, max_width))
        .collect()
}

/// プレビュー用チャンクファイルのディレクトリパスを生成
pub(super) fn preview_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("sqsh_preview_{}", std::process::id()))
}

const PREVIEW_CHUNK_SIZE: usize = 1000;

/// SQL実行履歴の最大保持件数
///
/// 超過した場合は最古のエントリを削除する
pub(super) const MAX_SQL_HISTORY: usize = 100;

/// プレビューデータを1行分チャンクバッファに追加し、チャンク境界でファイルに書き出す
///
/// チャンクファイル: preview_dir/chunk_0.txt (行0-999), chunk_1.txt (行1000-1999), ...
/// 各チャンク内は `---\n` 区切りのセクション形式
pub(super) fn append_preview_to_chunk(
    dir: &std::path::Path,
    row_index: usize,
    columns: &[String],
    data: &[String],
    chunk_buf: &mut String,
) {
    for (col_idx, cell) in data.iter().enumerate() {
        let col_name = columns.get(col_idx).map(|s| s.as_str()).unwrap_or("?");
        chunk_buf.push_str(col_name);
        chunk_buf.push_str(": ");
        chunk_buf.push_str(cell);
        chunk_buf.push('\n');
    }
    chunk_buf.push_str("---\n");

    // チャンク境界に達したらファイルに書き出してバッファをクリア
    if (row_index + 1) % PREVIEW_CHUNK_SIZE == 0 {
        let chunk_idx = row_index / PREVIEW_CHUNK_SIZE;
        if let Err(e) = std::fs::write(
            dir.join(format!("chunk_{}.txt", chunk_idx)),
            chunk_buf.as_str(),
        ) {
            // 書き込み失敗時はプレビューが表示されないだけで致命的ではないためwarnログに留める
            tracing::warn!(
                "プレビューチャンクの書き込みに失敗しました (chunk={}): {}",
                chunk_idx,
                e
            );
        }
        chunk_buf.clear();
    }
}

/// チャンクバッファの残りをファイルに書き出す（最終チャンク）
pub(super) fn flush_preview_chunk(dir: &std::path::Path, row_index: usize, chunk_buf: &str) {
    if !chunk_buf.is_empty() {
        let chunk_idx = row_index / PREVIEW_CHUNK_SIZE;
        if let Err(e) = std::fs::write(dir.join(format!("chunk_{}.txt", chunk_idx)), chunk_buf) {
            // 書き込み失敗時はプレビューが表示されないだけで致命的ではないためwarnログに留める
            tracing::warn!(
                "最終プレビューチャンクの書き込みに失敗しました (chunk={}): {}",
                chunk_idx,
                e
            );
        }
    }
}

/// プレビュー用のシェルコマンドを生成
///
/// アイテムテキストの先頭フィールド（スペース区切り）から元の行インデックスを取得し、
/// チャンクファイルを特定して awk でセクションを抽出する。
/// フィルタ後もインデックスがずれない。
pub(super) fn build_preview_cmd(dir: &std::path::Path, table_name: Option<&str>) -> String {
    // パス内のシングルクォートを '\'' でエスケープすることで
    // シングルクォート囲みのシェル文字列内でも安全に使用できる
    let dir_escaped = dir.display().to_string().replace('\'', "'\\''");
    // テーブル名がある場合はプレビューのトップにヘッダーとして表示する
    let header_part = match table_name {
        Some(name) => {
            let name_escaped = name.replace('\'', "'\\''");
            format!("echo '[Table: {}]'; echo ''; ", name_escaped)
        }
        None => String::new(),
    };
    // {} はskimがoutput()の値（行インデックスの数値のみ）に置換する
    format!(
        "IDX={{}}; CHUNK=$((IDX / {chunk})); OFF=$((IDX % {chunk})); \
         FILE='{dir_escaped}/chunk_'$CHUNK'.txt'; \
         {header_part}\
         [ -f \"$FILE\" ] && awk -v idx=$OFF 'BEGIN{{RS=\"---\\n\"}} NR==idx+1{{printf \"%s\", $0}}' \"$FILE\" || echo '(読み込み中...)'",
        chunk = PREVIEW_CHUNK_SIZE,
        dir_escaped = dir_escaped,
        header_part = header_part
    )
}

/// チャンクファイルから指定行のデータを読み出す
///
/// `append_preview_to_chunk` が書き出すフォーマット（`col_name: value\n` + `---\n` 区切り）を
/// パースして各カラムの値を Vec<String> として返す。
/// `all_rows` をメモリ上に保持せずに済むため、数百万行のクエリ結果でも OOM にならない。
pub(super) fn read_row_from_chunk(
    dir: &std::path::Path,
    row_index: usize,
    columns: &[String],
) -> crate::error::Result<Vec<String>> {
    let chunk_idx = row_index / PREVIEW_CHUNK_SIZE;
    let row_in_chunk = row_index % PREVIEW_CHUNK_SIZE;

    let chunk_path = dir.join(format!("chunk_{}.txt", chunk_idx));
    let content = std::fs::read_to_string(&chunk_path).map_err(|e| {
        crate::error::Error::Other(format!(
            "チャンクファイルの読み込みに失敗しました (row={}): {}",
            row_index, e
        ))
    })?;

    // `---\n` で区切られたレコードの中から対象行を取得する
    // split("---\n") は末尾の区切り文字の後に空文字列要素を生成するため、
    // 空のレコードは存在しない行として扱う
    let records: Vec<&str> = content.split("---\n").filter(|r| !r.is_empty()).collect();
    let record = records.get(row_in_chunk).ok_or_else(|| {
        crate::error::Error::Other(format!(
            "選択された行がチャンクファイル内に見つかりません (row={}, chunk={})",
            row_index, chunk_idx
        ))
    })?;

    // `col_name: value\n` 形式の各行からvalueを抽出する
    // カラム名に ": " が含まれる可能性を考慮し、カラム名リストを使って先頭マッチで分割する
    let mut values: Vec<String> = Vec::with_capacity(columns.len());
    let lines: Vec<&str> = record.lines().collect();

    for (i, col_name) in columns.iter().enumerate() {
        let prefix = format!("{}: ", col_name);
        if let Some(line) = lines.get(i) {
            if let Some(value) = line.strip_prefix(&prefix) {
                values.push(value.to_string());
            } else {
                // プレフィックスが一致しない場合（カラム名に":"が含まれる等のエッジケース）
                // ": " の最初の出現位置で分割するフォールバック
                if let Some(colon_pos) = line.find(": ") {
                    values.push(line[colon_pos + 2..].to_string());
                } else {
                    values.push(line.to_string());
                }
            }
        } else {
            values.push(String::new());
        }
    }

    Ok(values)
}

/// プレビュー用ディレクトリを削除
pub(super) fn cleanup_preview_dir(dir: &std::path::Path) {
    if let Err(e) = std::fs::remove_dir_all(dir) {
        // 一時ファイルの削除失敗は動作に影響しないためwarnログに留める
        tracing::warn!("プレビュー用ディレクトリの削除に失敗しました: {}", e);
    }
}

/// データ行を表示幅に合わせてフォーマットする
pub(super) fn format_row_display(row: &[String], col_widths: &[usize]) -> String {
    row.iter()
        .enumerate()
        .map(|(i, cell)| {
            let w = col_widths.get(i).copied().unwrap_or(10);
            pad_to_width(cell, w)
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// 結果表示用のskimオプションを構築する
///
/// no_mouse(true): skimのマウスイベント処理を無効化することで、ターミナルネイティブの
/// テキスト選択（マウスドラッグ→Cmd+C）を可能にする。キーボード操作は引き続き利用できる。
pub(super) fn build_result_skim_options<'a>(
    header_line: &'a str,
    preview_cmd: &'a str,
    prompt: &'a str,
) -> std::result::Result<SkimOptions<'a>, crate::error::Error> {
    SkimOptionsBuilder::default()
        .height(Some("100%"))
        .multi(false)
        .reverse(true)
        .header(Some(header_line))
        .prompt(Some(prompt))
        .preview(Some(preview_cmd))
        .preview_window(Some("right:30%:wrap"))
        .no_mouse(true)
        .build()
        .map_err(|e| crate::error::Error::Other(format!("{}: {:?}", t!(TuiMsg::SkimInitError), e)))
}

/// skimで選択された行からアクションを決定する
///
/// SHOW DATABASES結果ならUSE、SHOW TABLES結果ならSELECT、
/// それ以外ならWHEREテンプレート付きのレコード選択を返す。
pub(super) fn determine_skim_action(
    first_column: &str,
    first_value: &str,
    columns: &[String],
    values: &[String],
    source_sql: &str,
) -> SkimAction {
    if first_column == "Database" {
        SkimAction::DrillDown(format!(
            "USE {}",
            crate::query::escape_identifier(first_value)
        ))
    } else if first_column.starts_with("Tables_in_") {
        SkimAction::DrillDown(format!(
            "SELECT * FROM {}",
            crate::query::escape_identifier(first_value)
        ))
    } else {
        let record = SelectedRecord {
            columns: columns
                .iter()
                .zip(values.iter())
                .map(|(col, val)| (col.clone(), val.clone()))
                .collect(),
        };
        // source_sqlからテーブル名を抽出してSELECT文のテンプレートを生成する
        // テーブル名が取得できない場合は "?" をフォールバックとして使う
        let table_name =
            crate::completion::extract_from_table(source_sql).unwrap_or_else(|| "?".to_string());
        // MySQLのエスケープルールに従い、バックスラッシュ→シングルクォートの順に処理する
        // 順序が重要: バックスラッシュを先にエスケープしないと、後のシングルクォートエスケープが壊れる
        let escaped_value = first_value.replace('\\', "\\\\").replace('\'', "\\'");
        let where_clause = format!(
            "SELECT * FROM {} WHERE {} = '{}'",
            crate::query::escape_identifier(&table_name),
            crate::query::escape_identifier(first_column),
            escaped_value
        );
        SkimAction::SelectRecord {
            where_template: where_clause,
            record,
        }
    }
}

/// SQL入力エリアとShell入力エリアのフォーカス状態
///
/// Tab キーで Sql → Shell → Prompt → Sql の順に循環する。
/// APIキー未設定時は Prompt をスキップして Sql → Shell → Sql の2段循環にする。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum InputFocus {
    /// SQL入力エリア（デフォルト）
    #[default]
    Sql,
    /// Shell入力エリア
    Shell,
    /// PROMPT入力エリア（Claude AI 連携）
    Prompt,
}

/// PROMPT 入力エリアの状態管理
///
/// Claude API へのリクエスト状態と入力テキストを管理する。
#[derive(Debug)]
pub(super) struct PromptInputState {
    /// 入力中のプロンプトテキスト
    pub text: String,
    /// カーソル位置（char単位）
    pub cursor_position: usize,
    /// テキスト選択開始位置（char単位、None=選択なし）
    ///
    /// Shift+矢印キーで選択範囲を設定する。cursor_positionと組み合わせて
    /// min(selection_start, cursor_position)..max(selection_start, cursor_position) が選択範囲となる。
    pub selection_start: Option<usize>,
    /// Ctrl+K / Ctrl+U で削除したテキストを保存するキルバッファ
    ///
    /// Ctrl+Y（yank）でペースト可能。システムクリップボードとは独立している。
    pub kill_buffer: String,
    /// API リクエスト処理中フラグ
    pub is_processing: bool,
    /// 最後のエラーメッセージ（None = エラーなし）
    pub last_error: Option<String>,
    /// ローディングアニメーションのフレームカウンター
    ///
    /// is_processing が true の間、イベントループのポーリングごとにインクリメントされ、
    /// 描画時に braille スピナーのフレーム選択に使用する。
    pub loading_tick: u8,
}

/// 実行中クエリの管理情報
pub(super) struct RunningQuery {
    manager: ConnectionManager,
    task: JoinHandle<Result<QueryResult>>,
}

/// アプリケーション状態
///
/// design.mdに基づく状態管理: 各状態がデータを保持
pub enum AppState {
    /// 接続先選択中
    Selecting {
        connections: Vec<crate::config::ConnectionConfig>,
        selected_index: usize,
    },

    /// 接続済み（SQL入力待ち）
    Connected { manager: ConnectionManager },

    /// クエリ実行中
    Executing { query: String },

    /// 結果表示中
    ShowingResult {
        result: QueryResult,
        /// エラー回復時に戻るConnectionManager
        manager: Option<ConnectionManager>,
    },

    /// ストリーミング結果表示待ち
    ///
    /// 表示系クエリ（USE/SET以外）はDBから行を取得しながら即座にskimに送信する。
    /// TUIループがこの状態を検出したら、一時停止→ストリーミング表示→再開を行う。
    StreamingQuery {
        manager: ConnectionManager,
        sql: String,
        /// クエリタイムアウト（MysqlConfig.timeoutから取得）
        timeout_secs: u64,
    },

    /// カラム選択中（TUI一時停止→skim起動→TUI再開）
    ///
    /// Ctrl+S または sc エイリアスで遷移する。
    /// TUIループがこの状態を検出したら、SHOW TABLES → テーブル選択 → SHOW COLUMNS → カラム選択 を行い、
    /// 生成した SELECT 文を query_input にセットして Connected 状態に戻る。
    SelectingColumns {
        manager: ConnectionManager,
        /// クエリタイムアウト（MysqlConfig.timeoutから取得）
        timeout_secs: u64,
    },

    /// エラー表示中
    Error {
        message: String,
        /// エラー発生前の状態（戻り先）
        previous_state: Box<AppState>,
    },
}

/// SQL入力エリアの状態管理
pub(super) struct SqlInputState {
    /// 入力中のSQLテキスト
    pub text: String,
    /// カーソル位置（char単位）
    pub cursor_position: usize,
    /// テキスト選択開始位置（char単位、None=選択なし）
    ///
    /// Shift+矢印キーで選択範囲を設定する。cursor_positionと組み合わせて
    /// min(selection_start, cursor_position)..max(selection_start, cursor_position) が選択範囲となる。
    pub selection_start: Option<usize>,
    /// 最後に実行したSQL（WHEREテンプレート生成時にテーブル名を抽出するために保持）
    ///
    /// ShowingResult 遷移時に text がクリアされるため、
    /// show_result_with_skim でテーブル名を参照できるよう別途保存する。
    pub last_sql: String,
    /// SQL実行履歴（最新が末尾）
    ///
    /// Enter実行時に追加し、直前と同じクエリは重複追加しない。
    /// MAX_SQL_HISTORY を超えた場合は先頭（最古）を削除する。
    /// 先頭削除がO(n)になるVecの代わりにVecDequeを使用する。
    pub history: VecDeque<String>,
    /// 履歴参照中の現在位置（None=新規入力中、Some(index)=履歴参照中）
    pub history_index: Option<usize>,
    /// 履歴参照を開始した時点で退避しておいた入力中テキスト
    ///
    /// ↓キーで履歴末尾を超えて新規入力状態に戻る際に復元する。
    pub history_draft: String,
    /// Ctrl+K / Ctrl+U で削除したテキストを保存するキルバッファ
    ///
    /// Ctrl+Y（yank）でペースト可能。システムクリップボードとは独立している。
    pub kill_buffer: String,
    /// 補完候補キャッシュ（接続確立後に非同期で充填）
    ///
    /// Arc<tokio::sync::RwLock<...>> でラップし、バックグラウンドタスクから
    /// 書き込み、TUIの描画ループから読み取りを安全に行う。
    pub completion_cache: Arc<tokio::sync::RwLock<CompletionCache>>,
    /// 補完ポップアップ状態
    ///
    /// None = ポップアップ非表示、Some(...) = 候補リスト表示中
    pub completion_state: Option<CompletionState>,
}

impl SqlInputState {
    fn new() -> Self {
        Self {
            text: String::new(),
            cursor_position: 0,
            selection_start: None,
            last_sql: String::new(),
            history: VecDeque::new(),
            history_index: None,
            history_draft: String::new(),
            kill_buffer: String::new(),
            completion_cache: Arc::new(tokio::sync::RwLock::new(CompletionCache::new())),
            completion_state: None,
        }
    }
}

/// Shell入力エリアの状態管理
#[derive(Debug)]
pub(super) struct ShellInputState {
    /// 入力中のテキスト
    pub text: String,
    /// カーソル位置（char単位）
    pub cursor_position: usize,
    /// テキスト選択開始位置（char単位、None=選択なし）
    ///
    /// Shift+矢印キーで選択範囲を設定する。cursor_positionと組み合わせて
    /// min(selection_start, cursor_position)..max(selection_start, cursor_position) が選択範囲となる。
    pub selection_start: Option<usize>,
    /// Ctrl+K / Ctrl+U で削除したテキストを保存するキルバッファ
    ///
    /// Ctrl+Y（yank）でペースト可能。システムクリップボードとは独立している。
    pub kill_buffer: String,
    /// Shell実行履歴（最新が末尾）
    ///
    /// MAX_SQL_HISTORY と同じ上限を使用する。
    pub history: VecDeque<String>,
    /// Shell履歴参照中の現在位置（None=新規入力中、Some(index)=履歴参照中）
    pub history_index: Option<usize>,
    /// Shell履歴参照を開始した時点で退避しておいた入力中テキスト
    ///
    /// ↓キーで履歴末尾を超えて新規入力状態に戻る際に復元する。
    pub history_draft: String,
    /// 実行待ちのシェルコマンド
    ///
    /// handle_shell_input から直接 terminal を操作できないため、
    /// run_loop が検出して TUI を一時停止しコマンドを実行する pending 方式を採用する。
    pub pending_command: Option<String>,
}

impl ShellInputState {
    fn new() -> Self {
        Self {
            text: String::new(),
            cursor_position: 0,
            selection_start: None,
            kill_buffer: String::new(),
            history: VecDeque::new(),
            history_index: None,
            history_draft: String::new(),
            pending_command: None,
        }
    }
}

/// 補完ポップアップの表示状態
#[derive(Debug, Clone)]
pub struct CompletionState {
    /// 現在表示中の補完候補リスト（フィルタ済み）
    pub candidates: Vec<CompletionItem>,
    /// 選択中の候補インデックス（0ベース）
    pub selected_index: usize,
    /// 現在入力中のトークン（ポップアップ表示開始時点のスナップショット）
    pub current_token: String,
}

/// TUIアプリケーション
pub struct App {
    /// 現在の状態
    pub(super) state: AppState,

    /// 終了フラグ
    pub(super) should_quit: bool,

    /// バックグラウンドで実行中のクエリ
    pub(super) running_query: Option<RunningQuery>,

    /// 選択されたレコードのプレビュー情報（SQL入力画面で表示）
    pub(super) selected_record: Option<SelectedRecord>,

    /// グレースフルシャットダウン用フラグ
    pub(super) shutdown_flag: Arc<AtomicBool>,

    /// USEコマンドで選択中のデータベース名
    ///
    /// 初期値はNone。USEコマンド成功時に更新される。
    /// 接続情報表示で「選択データベース: xxx」として表示する。
    pub(super) current_database: Option<String>,

    /// 接続先の名前（パンくずリスト表示用）
    ///
    /// 接続確立時（Selecting→Connected遷移時）に設定される。
    /// 切断するまで変化しない。
    pub(super) connection_name: Option<String>,

    /// bastion経由接続時のbastionホスト名（パンくずリスト表示用）
    ///
    /// bastion経由でない場合はNone。接続確立時に設定され、切断するまで変化しない。
    pub(super) bastion_name: Option<String>,

    /// 現在操作中のテーブル名（パンくずリスト表示用）
    ///
    /// SELECT文実行時やCtrl+Sでテーブル選択時に更新される。
    /// USEコマンドでDB切り替え時にクリアされる。
    pub(super) current_table: Option<String>,

    /// readonlyモードフラグ（CLI --readonly または接続設定 readonly=true）
    ///
    /// CLIフラグが true の場合は全接続をreadonly強制する。
    /// 接続設定の readonly=true との論理和で最終的な判定を行う。
    pub(super) readonly: bool,

    /// SQL/Shell/Prompt 入力エリアのフォーカス状態
    ///
    /// Tab キーで Sql → Shell → Prompt → Sql の順に循環する。
    /// APIキー未設定時は Prompt をスキップして Sql → Shell → Sql の2段循環にする。
    pub(super) input_focus: InputFocus,

    /// SQL入力エリアの状態
    pub(super) sql: SqlInputState,

    /// Shell入力エリアの状態
    pub(super) shell: ShellInputState,

    /// アプリケーション設定（anthropic_api_key, claude_model 等）
    pub(super) settings: crate::config::AppSettings,

    /// PROMPT 入力エリアの状態（テキスト・カーソル・処理中フラグ・エラーなど）
    pub(super) prompt: PromptInputState,

    /// PROMPT バックグラウンドタスクのハンドル
    ///
    /// Enter で claude::run_agent を spawn し、完了時に poll_prompt_completion() で
    /// 生成 SQL を sql.text に書き込む。
    pub(super) prompt_task: Option<JoinHandle<crate::error::Result<String>>>,
}

impl App {
    /// 新しいアプリケーションを作成
    pub fn new(config: Config, shutdown_flag: Arc<AtomicBool>, cli_readonly: bool) -> Self {
        // default_bastionを適用した接続設定リストを取得
        let connections = config.resolve_connections();
        // settings は Config から取り出す（anthropic_api_key, claude_model 等を保持）
        let settings = config.settings;
        Self {
            state: AppState::Selecting {
                connections,
                selected_index: 0,
            },
            should_quit: false,
            running_query: None,
            selected_record: None,
            shutdown_flag,
            current_database: None,
            connection_name: None,
            bastion_name: None,
            current_table: None,
            readonly: cli_readonly,
            input_focus: InputFocus::default(),
            sql: SqlInputState::new(),
            shell: ShellInputState::new(),
            settings,
            prompt: PromptInputState {
                text: String::new(),
                cursor_position: 0,
                selection_start: None,
                kill_buffer: String::new(),
                is_processing: false,
                last_error: None,
                loading_tick: 0,
            },
            prompt_task: None,
        }
    }

    /// アプリケーションのメインループを実行
    pub async fn run(&mut self) -> Result<()> {
        // 接続先選択（Selecting状態の場合のみ）
        if let AppState::Selecting {
            ref connections, ..
        } = self.state
        {
            let selected_connection = crate::selector::select_connection(connections)?;

            // CLIフラグとTOML設定の論理和でreadonly判定
            // CLI --readonly が true の場合は全接続に適用し、TOML設定のreadonly=trueも尊重する
            let readonly = self.readonly || selected_connection.readonly;

            // 接続名をパンくずリスト表示用に先に保存する（connect でムーブされるため）
            self.connection_name = Some(selected_connection.name.clone());

            // bastion経由の場合はbastionホスト名をパンくず表示用に保存する（connect でムーブされるため）
            // resolve_connections() 後は BastionSetting::Config のみ存在する
            self.bastion_name = match &selected_connection.bastion {
                Some(crate::config::BastionSetting::Config(ref cfg)) => Some(cfg.host.clone()),
                _ => None,
            };

            // 接続を確立
            tracing::info!("Connecting to: {}", selected_connection.name);
            let manager =
                crate::connection::ConnectionManager::connect(selected_connection, readonly)
                    .await?;

            // Connected状態に遷移
            self.state = AppState::Connected { manager };

            // バックグラウンドでキャッシュを初期化する（補完機能のためのSHOW TABLES/DATABASES）
            // 失敗しても補完なしで動作継続するため、エラーはwarnログのみ
            // 初回接続時は current_database は None なので None を渡す
            if let AppState::Connected { ref manager } = self.state {
                let cache_arc = self.sql.completion_cache.clone();
                let pool = manager.pool().clone();
                tokio::spawn(async move {
                    if let Err(e) = initialize_completion_cache(&cache_arc, &pool, None).await {
                        tracing::warn!("補完キャッシュの初期化に失敗しました: {}", e);
                    }
                });
            }
        }

        // ターミナル初期化
        enable_raw_mode().map_err(|e| Error::Tui(format!("ターミナル初期化失敗: {}", e)))?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)
            .map_err(|e| Error::Tui(format!("ターミナル初期化失敗: {}", e)))?;

        let backend = CrosstermBackend::new(stdout);
        let mut terminal =
            Terminal::new(backend).map_err(|e| Error::Tui(format!("ターミナル作成失敗: {}", e)))?;

        // メインループ
        let result = self.run_loop(&mut terminal).await;

        // ターミナル復元
        disable_raw_mode().map_err(|e| Error::Tui(format!("ターミナル復元失敗: {}", e)))?;
        execute!(terminal.backend_mut(), LeaveAlternateScreen)
            .map_err(|e| Error::Tui(format!("ターミナル復元失敗: {}", e)))?;
        terminal
            .show_cursor()
            .map_err(|e| Error::Tui(format!("カーソル表示失敗: {}", e)))?;

        result
    }

    /// メインイベントループ
    async fn run_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<()> {
        loop {
            self.poll_query_completion().await?;
            self.poll_prompt_completion().await;

            // StreamingQuery状態に遷移した場合、ストリーミングでskimに渡す
            if matches!(self.state, AppState::StreamingQuery { .. }) {
                let (manager, sql, timeout_secs) = match std::mem::replace(
                    &mut self.state,
                    AppState::Selecting {
                        connections: Vec::new(),
                        selected_index: 0,
                    },
                ) {
                    AppState::StreamingQuery {
                        manager,
                        sql,
                        timeout_secs,
                    } => (manager, sql, timeout_secs),
                    other => {
                        self.state = other;
                        continue;
                    }
                };

                // TUI一時停止
                disable_raw_mode().map_err(|e| Error::Tui(format!("ターミナル復元失敗: {}", e)))?;
                execute!(terminal.backend_mut(), LeaveAlternateScreen)
                    .map_err(|e| Error::Tui(format!("ターミナル復元失敗: {}", e)))?;

                // ストリーミング表示（SQLエラー・タイムアウト時は?でErrを返してrun_loopに伝播）
                let streaming_result = self.show_result_streaming(
                    manager.pool().clone(),
                    &sql,
                    std::time::Duration::from_secs(timeout_secs),
                );

                // TUI再開（ストリーミング結果に関わらず必ず再開する）
                enable_raw_mode()
                    .map_err(|e| Error::Tui(format!("ターミナル初期化失敗: {}", e)))?;
                execute!(terminal.backend_mut(), EnterAlternateScreen)
                    .map_err(|e| Error::Tui(format!("ターミナル初期化失敗: {}", e)))?;
                terminal
                    .clear()
                    .map_err(|e| Error::Tui(format!("画面クリア失敗: {}", e)))?;

                // SQLエラー・タイムアウト発生時はError状態に遷移してSQL入力画面に戻れるようにする
                let next_query = match streaming_result {
                    Err(e) => {
                        tracing::error!("Streaming query failed: {}", e);
                        self.state = AppState::Error {
                            message: format!("{}", e),
                            previous_state: Box::new(AppState::Connected { manager }),
                        };
                        continue;
                    }
                    Ok(action) => action,
                };

                match next_query {
                    Some(SkimAction::DrillDown(next_sql)) => {
                        self.state = AppState::Connected { manager };
                        self.selected_record = None;
                        self.sql.text = next_sql;
                        self.sql.cursor_position = self.sql.text.chars().count();
                        self.add_to_history(&self.sql.text.clone());
                        let sql_upper = self.sql.text.trim().to_uppercase();
                        if sql_upper.starts_with("USE ") || sql_upper.starts_with("SET ") {
                            self.execute_query()?;
                        } else {
                            self.transition_to_streaming()?;
                        }
                    }
                    Some(SkimAction::SelectRecord {
                        where_template,
                        record,
                    }) => {
                        self.state = AppState::Connected { manager };
                        self.selected_record = Some(record);
                        self.sql.text = where_template;
                        self.sql.cursor_position = self.sql.text.chars().count();
                    }
                    None => {
                        self.state = AppState::Connected { manager };
                        self.sql.text.clear();
                        self.sql.cursor_position = 0;
                    }
                }

                continue;
            }

            // SelectingColumns状態に遷移した場合、skimでカラム選択
            if matches!(self.state, AppState::SelectingColumns { .. }) {
                let (manager, timeout_secs) = match std::mem::replace(
                    &mut self.state,
                    AppState::Selecting {
                        connections: Vec::new(),
                        selected_index: 0,
                    },
                ) {
                    AppState::SelectingColumns {
                        manager,
                        timeout_secs,
                    } => (manager, timeout_secs),
                    other => {
                        self.state = other;
                        continue;
                    }
                };

                // TUI一時停止
                disable_raw_mode().map_err(|e| Error::Tui(format!("ターミナル復元失敗: {}", e)))?;
                execute!(terminal.backend_mut(), LeaveAlternateScreen)
                    .map_err(|e| Error::Tui(format!("ターミナル復元失敗: {}", e)))?;

                // カラム選択（DBエラー時は Error 状態に遷移）
                // current_database を渡して USE 後のDBのテーブル一覧を正しく表示する
                let select_result = self.select_columns_interactive(
                    manager.pool(),
                    std::time::Duration::from_secs(timeout_secs),
                    self.current_database.as_deref(),
                );

                // TUI再開（カラム選択結果に関わらず必ず再開する）
                enable_raw_mode()
                    .map_err(|e| Error::Tui(format!("ターミナル初期化失敗: {}", e)))?;
                execute!(terminal.backend_mut(), EnterAlternateScreen)
                    .map_err(|e| Error::Tui(format!("ターミナル初期化失敗: {}", e)))?;
                terminal
                    .clear()
                    .map_err(|e| Error::Tui(format!("画面クリア失敗: {}", e)))?;

                match select_result {
                    Err(e) => {
                        tracing::error!("Column selection failed: {}", e);
                        self.state = AppState::Error {
                            message: format!("{}", e),
                            previous_state: Box::new(AppState::Connected { manager }),
                        };
                    }
                    Ok(Some(sql)) => {
                        // 生成されたSELECT文を即実行する
                        self.state = AppState::Connected { manager };
                        self.sql.text = sql;
                        self.sql.cursor_position = self.sql.text.chars().count();
                        self.execute_query()?;
                    }
                    Ok(None) => {
                        // キャンセル: Connected 状態に戻るだけ
                        self.state = AppState::Connected { manager };
                    }
                }

                continue;
            }

            // ShowingResult状態に遷移した場合、skimで結果を表示
            if matches!(self.state, AppState::ShowingResult { .. }) {
                // 状態からresultとmanagerを取り出す
                let (result, manager_opt) = match std::mem::replace(
                    &mut self.state,
                    AppState::Selecting {
                        connections: Vec::new(),
                        selected_index: 0,
                    },
                ) {
                    AppState::ShowingResult { result, manager } => (result, manager),
                    other => {
                        self.state = other;
                        continue;
                    }
                };

                // ratatuiを一時停止
                disable_raw_mode().map_err(|e| Error::Tui(format!("ターミナル復元失敗: {}", e)))?;
                execute!(terminal.backend_mut(), LeaveAlternateScreen)
                    .map_err(|e| Error::Tui(format!("ターミナル復元失敗: {}", e)))?;

                // skimで結果表示
                let next_query = self.show_result_with_skim(&result)?;

                // ratatuiを再開
                enable_raw_mode()
                    .map_err(|e| Error::Tui(format!("ターミナル初期化失敗: {}", e)))?;
                execute!(terminal.backend_mut(), EnterAlternateScreen)
                    .map_err(|e| Error::Tui(format!("ターミナル初期化失敗: {}", e)))?;
                terminal
                    .clear()
                    .map_err(|e| Error::Tui(format!("画面クリア失敗: {}", e)))?;

                // 結果に応じて状態遷移
                match next_query {
                    Some(SkimAction::DrillDown(sql)) => {
                        if let Some(manager) = manager_opt {
                            self.state = AppState::Connected { manager };
                            self.selected_record = None;
                            self.sql.text = sql;
                            self.sql.cursor_position = self.sql.text.chars().count();
                            self.add_to_history(&self.sql.text.clone());
                            self.execute_query()?;
                        } else {
                            self.should_quit = true;
                        }
                    }
                    Some(SkimAction::SelectRecord {
                        where_template,
                        record,
                    }) => {
                        if let Some(manager) = manager_opt {
                            self.state = AppState::Connected { manager };
                            self.selected_record = Some(record);
                            self.sql.text = where_template;
                            self.sql.cursor_position = self.sql.text.chars().count();
                        } else {
                            self.should_quit = true;
                        }
                    }
                    None => {
                        if let Some(manager) = manager_opt {
                            self.state = AppState::Connected { manager };
                        } else {
                            self.should_quit = true;
                        }
                    }
                }

                continue;
            }

            // AI処理中はローディングアニメーションのカウンターを更新する
            // ポーリングループ（100ms間隔）ごとにインクリメントすることで
            // 描画時に braille スピナーのフレームが自然に切り替わる
            if self.prompt.is_processing {
                self.prompt.loading_tick = self.prompt.loading_tick.wrapping_add(1);
            }

            // 画面描画
            terminal
                .draw(|f| self.render(f))
                .map_err(|e| Error::Tui(format!("描画エラー: {}", e)))?;

            // シャットダウンフラグチェック
            if self.shutdown_flag.load(Ordering::Relaxed) {
                tracing::info!("Shutdown signal received, waiting for ongoing operations...");

                // 実行中のクエリがある場合は最大5秒待機
                if matches!(self.state, AppState::Executing { .. }) {
                    tracing::info!("Query is executing, waiting up to 5 seconds...");

                    let start = std::time::Instant::now();
                    while matches!(self.state, AppState::Executing { .. })
                        && start.elapsed() < std::time::Duration::from_secs(5)
                    {
                        self.poll_query_completion().await?;
                        if matches!(self.state, AppState::Executing { .. }) {
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                    }

                    if matches!(self.state, AppState::Executing { .. }) {
                        tracing::warn!("Query execution timed out during shutdown, aborting task");
                        self.abort_running_query();
                    } else {
                        tracing::info!("Query completed successfully before shutdown");
                    }
                }

                self.should_quit = true;
            }

            // 終了チェック
            if self.should_quit {
                self.abort_running_query();
                break;
            }

            // pending_shell_command チェック: TUI を一時停止してシェルコマンドを実行する
            // handle_shell_input は terminal への参照を持てないため、
            // App フィールド経由でトリガーを通知し、run_loop 側で実際の停止・再起動を担う
            if let Some(cmd) = self.shell.pending_command.take() {
                use std::process::Stdio;

                // bastion経由接続中の場合はbastionサーバー上でコマンドを実行する。
                // resolve_connections()適用後のConfigではbastionはConfig(BastionConfig)かNoneのみなので
                // Toggle(true/false)は考慮不要。
                let bastion_config: Option<BastionConfig> = self
                    .current_connection_config()
                    .and_then(|config| match &config.bastion {
                        Some(BastionSetting::Config(cfg)) => Some(cfg.clone()),
                        _ => None,
                    });

                if bastion_config.is_some() {
                    tracing::info!("Executing shell command on bastion server: {}", cmd);
                } else {
                    tracing::info!("Executing shell command locally: {}", cmd);
                }

                // TUI を一時停止
                disable_raw_mode().map_err(|e| Error::Tui(format!("ターミナル復元失敗: {}", e)))?;
                execute!(terminal.backend_mut(), LeaveAlternateScreen)
                    .map_err(|e| Error::Tui(format!("ターミナル復元失敗: {}", e)))?;

                let status = if let Some(ref bastion_cfg) = bastion_config {
                    // bastion経由: ssh コマンド経由でリモート実行する
                    let mut ssh_cmd = tokio::process::Command::new("ssh");
                    ssh_cmd
                        .arg("-p")
                        .arg(bastion_cfg.port.to_string())
                        .arg(format!("{}@{}", bastion_cfg.user, bastion_cfg.host));

                    // key_pathが指定されている場合のみ -i オプションを付ける。
                    // 指定がない場合は SSH agent に委ねる。
                    if let Some(ref key_path) = bastion_cfg.key_path {
                        ssh_cmd.arg("-i").arg(key_path);
                    }

                    ssh_cmd
                        .arg(&cmd)
                        .stdin(Stdio::inherit())
                        .stdout(Stdio::inherit())
                        .stderr(Stdio::inherit())
                        .status()
                        .await
                        .map_err(|e| Error::Tui(format!("SSHコマンド実行失敗: {}", e)))?
                } else {
                    // 直接接続: sh -c でローカル実行する（標準 I/O を継承）
                    tokio::process::Command::new("sh")
                        .arg("-c")
                        .arg(&cmd)
                        .stdin(Stdio::inherit())
                        .stdout(Stdio::inherit())
                        .stderr(Stdio::inherit())
                        .status()
                        .await
                        .map_err(|e| Error::Tui(format!("シェルコマンド実行失敗: {}", e)))?
                };

                if !status.success() {
                    tracing::warn!("Shell command exited with status: {}", status);
                }

                // ユーザーが結果を確認できるよう Enter 入力まで待機
                println!("\n[Press Enter to continue...]");
                let _ = std::io::stdin().read_line(&mut String::new());

                // TUI を再開
                enable_raw_mode()
                    .map_err(|e| Error::Tui(format!("ターミナル初期化失敗: {}", e)))?;
                execute!(terminal.backend_mut(), EnterAlternateScreen)
                    .map_err(|e| Error::Tui(format!("ターミナル初期化失敗: {}", e)))?;
                terminal
                    .clear()
                    .map_err(|e| Error::Tui(format!("画面クリア失敗: {}", e)))?;

                continue;
            }

            // イベント処理（100ms待機）
            if event::poll(std::time::Duration::from_millis(100))
                .map_err(|e| Error::Tui(format!("イベント取得失敗: {}", e)))?
            {
                let ev = event::read()
                    .map_err(|e| Error::Tui(format!("イベント読み込み失敗: {}", e)))?;
                self.handle_event(ev).await?;
            }
        }

        Ok(())
    }

    /// 現在接続中のConnectionConfigを取得する（bastion判定に使用）
    ///
    /// manager を保持しているすべての AppState からconfig参照を返す。
    /// 接続していない状態（Selecting / Executing / Error 等）ではNoneを返す。
    fn current_connection_config(&self) -> Option<&crate::config::ConnectionConfig> {
        match &self.state {
            AppState::Connected { manager } => Some(manager.config()),
            AppState::StreamingQuery { manager, .. } => Some(manager.config()),
            AppState::SelectingColumns { manager, .. } => Some(manager.config()),
            AppState::ShowingResult {
                manager: Some(manager),
                ..
            } => Some(manager.config()),
            _ => None,
        }
    }

    /// 現在の接続がreadonlyモードかどうかを返す
    ///
    /// Connected状態ではConnectionManagerのis_readonly()を参照する。
    /// それ以外の状態ではCLIフラグ由来のself.readonlyを返す。
    fn is_current_readonly(&self) -> bool {
        match &self.state {
            AppState::Connected { manager } => manager.is_readonly(),
            _ => self.readonly,
        }
    }

    /// クエリを実行
    fn execute_query(&mut self) -> Result<()> {
        // AppStateからmanagerをムーブ
        let manager = match std::mem::replace(
            &mut self.state,
            AppState::Executing {
                query: String::new(),
            },
        ) {
            AppState::Connected { manager } => manager,
            AppState::ShowingResult {
                manager: Some(manager),
                ..
            } => manager,
            other => {
                // 元の状態に戻す
                self.state = other;
                return Err(Error::Other("接続がありません".to_string()));
            }
        };

        let query = self.sql.text.clone();
        let pool = manager.pool().clone();
        let query_for_task = query.clone();
        // プールのセッション状態問題を回避するため、現在のデータベースをキャプチャしておく。
        // クエリ実行は別タスクで行われるため、クロージャにムーブする必要がある。
        let current_database_for_task = self.current_database.clone();

        // 次の show_result_with_skim でテーブル名を抽出できるよう保存する
        self.sql.last_sql = query.clone();

        // 実行中状態に遷移
        self.state = AppState::Executing {
            query: query.clone(),
        };
        self.running_query = Some(RunningQuery {
            manager,
            // TUIの再描画と入力処理を止めないため、クエリは別タスクで実行する
            task: tokio::spawn(async move {
                crate::query::execute_query(
                    &pool,
                    &query_for_task,
                    current_database_for_task.as_deref(),
                )
                .await
            }),
        });

        Ok(())
    }

    /// 実行中クエリの完了を取り込み、状態遷移を進める
    async fn poll_query_completion(&mut self) -> Result<()> {
        let task_finished = self
            .running_query
            .as_ref()
            .is_some_and(|running_query| running_query.task.is_finished());

        if !task_finished {
            return Ok(());
        }

        let Some(running_query) = self.running_query.take() else {
            return Ok(());
        };

        let query = match std::mem::replace(
            &mut self.state,
            AppState::Selecting {
                connections: Vec::new(),
                selected_index: 0,
            },
        ) {
            AppState::Executing { query } => query,
            other => {
                self.state = other;
                self.running_query = Some(running_query);
                return Ok(());
            }
        };

        let RunningQuery { manager, task } = running_query;

        match task.await {
            Ok(Err(e)) => {
                tracing::error!("Query execution failed: {}", e);
                let error_message = t!(TuiMsg::QueryFailed {
                    detail: &e.user_message()
                });
                let previous_state = Box::new(AppState::Connected { manager });
                self.state = AppState::Error {
                    message: error_message,
                    previous_state,
                };
                Ok(())
            }
            Ok(Ok(result)) => {
                if result.should_display {
                    // 結果表示状態に遷移（managerを保持）
                    self.state = AppState::ShowingResult {
                        result,
                        manager: Some(manager),
                    };
                    self.sql.text.clear();
                    self.sql.cursor_position = 0;
                } else {
                    // USE/SET等の結果を表示しないコマンドは即座にConnected状態に戻る
                    // USEコマンドの場合は選択データベースを更新する
                    self.update_current_database();
                    tracing::debug!("Command executed, returning to Connected state");
                    self.state = AppState::Connected { manager };
                    self.sql.text.clear();
                    self.sql.cursor_position = 0;

                    // USE実行後はテーブルキャッシュを更新する（新しいDBのテーブル一覧を取得）
                    // self.current_database は update_current_database() で更新済みのため、
                    // クローンして spawn に渡すことで正しいDBのテーブル一覧を取得できる
                    if let AppState::Connected { ref manager } = self.state {
                        let cache_arc = self.sql.completion_cache.clone();
                        let pool = manager.pool().clone();
                        let current_db = self.current_database.clone();
                        tokio::spawn(async move {
                            if let Err(e) =
                                refresh_table_cache(&cache_arc, &pool, current_db.as_deref()).await
                            {
                                tracing::warn!("テーブルキャッシュの更新に失敗しました: {}", e);
                            }
                        });
                    }
                }
                Ok(())
            }
            Err(join_error) => {
                tracing::error!(
                    "Query execution task failed for '{}': {}",
                    query,
                    join_error
                );
                let error_message = if join_error.is_cancelled() {
                    t!(TuiMsg::QueryCancelled { query: &query })
                } else {
                    t!(TuiMsg::QueryTaskFailed {
                        detail: &join_error.to_string()
                    })
                };
                let previous_state = Box::new(AppState::Connected { manager });
                self.state = AppState::Error {
                    message: error_message,
                    previous_state,
                };
                Ok(())
            }
        }
    }

    /// Shell実行履歴に追加する
    ///
    /// 直前と同じコマンドは重複追加しない。最大MAX_SQL_HISTORY件を保持する。
    pub(super) fn add_to_shell_history(&mut self, cmd: &str) {
        let cmd = cmd.trim().to_string();
        if cmd.is_empty() {
            return;
        }
        if self.shell.history.back().map(|s| s.as_str()) != Some(&cmd) {
            self.shell.history.push_back(cmd);
            if self.shell.history.len() > MAX_SQL_HISTORY {
                self.shell.history.pop_front();
            }
        }
        // 履歴参照状態をリセット（実行後は新規入力状態に戻す）
        self.shell.history_index = None;
        self.shell.history_draft.clear();
    }

    /// Shell履歴を遡る（古い方向へ）
    pub(super) fn shell_history_prev(&mut self) {
        if self.shell.history.is_empty() {
            return;
        }
        match self.shell.history_index {
            None => {
                // 新規入力中 → 現在の入力を退避して最新の履歴を表示
                self.shell.history_draft = self.shell.text.clone();
                let idx = self.shell.history.len() - 1;
                self.shell.history_index = Some(idx);
                self.shell.text = self.shell.history[idx].clone();
            }
            Some(idx) if idx > 0 => {
                // 履歴参照中 → さらに古い履歴へ
                let new_idx = idx - 1;
                self.shell.history_index = Some(new_idx);
                self.shell.text = self.shell.history[new_idx].clone();
            }
            _ => {
                // 最古の履歴に到達済み → 何もしない
                return;
            }
        }
        self.shell.cursor_position = self.shell.text.chars().count();
    }

    /// Shell履歴を進む（新しい方向へ）
    pub(super) fn shell_history_next(&mut self) {
        match self.shell.history_index {
            Some(idx) => {
                if idx + 1 < self.shell.history.len() {
                    // より新しい履歴へ
                    let new_idx = idx + 1;
                    self.shell.history_index = Some(new_idx);
                    self.shell.text = self.shell.history[new_idx].clone();
                } else {
                    // 履歴の末尾を超えた → 退避した入力を復元して新規入力状態に戻す
                    self.shell.history_index = None;
                    self.shell.text = self.shell.history_draft.clone();
                    self.shell.history_draft.clear();
                }
                self.shell.cursor_position = self.shell.text.chars().count();
            }
            None => {
                // 新規入力中 → 何もしない
            }
        }
    }

    /// 実行中クエリがあれば中断する
    fn abort_running_query(&mut self) {
        if let Some(running_query) = self.running_query.take() {
            tracing::info!("Aborting running query task");
            running_query.task.abort();
        }
    }

    /// PROMPT バックグラウンドタスクの完了をポーリングして結果を反映する
    ///
    /// 完了時に生成 SQL を sql.text に書き込む:
    /// - 成功時: sql.text に生成 SQL をセットし、フォーカスを Sql に戻す
    /// - エラー時: prompt.last_error にエラーメッセージをセットする
    /// - いずれの場合も is_processing = false にリセットする
    /// - エージェントが内部で実行した SELECT は別接続で完結するため
    ///   TUI 側のクエリ結果テーブルには影響しない
    pub(super) async fn poll_prompt_completion(&mut self) {
        let task_finished = self.prompt_task.as_ref().is_some_and(|t| t.is_finished());

        if !task_finished {
            return;
        }

        let Some(task) = self.prompt_task.take() else {
            return;
        };

        match task.await {
            Ok(Ok(sql)) => {
                tracing::debug!("PROMPT task completed: generated SQL length={}", sql.len());
                // TUI の SQL 入力エリアは1行表示のため、改行をスペースに変換してワンライナーにする
                let sql = normalize_sql_to_oneliner(&sql);
                self.sql.text = sql;
                self.sql.cursor_position = self.sql.text.chars().count();
                self.prompt.is_processing = false;
                self.prompt.last_error = None;
                // アニメーションを停止するためカウンターをリセットする
                self.prompt.loading_tick = 0;
                // 生成 SQL を確認しやすいよう SQL 入力エリアにフォーカスを戻す
                self.input_focus = InputFocus::Sql;
            }
            Ok(Err(e)) => {
                tracing::error!("PROMPT task failed: {}", e);
                self.prompt.last_error = Some(e.user_message());
                self.prompt.is_processing = false;
                // アニメーションを停止するためカウンターをリセットする
                self.prompt.loading_tick = 0;
            }
            Err(join_error) => {
                tracing::error!("PROMPT task panicked: {}", join_error);
                self.prompt.last_error = Some(format!("タスクが異常終了しました: {}", join_error));
                self.prompt.is_processing = false;
                // アニメーションを停止するためカウンターをリセットする
                self.prompt.loading_tick = 0;
            }
        }
    }
}

/// SQL 文字列を TUI 入力欄用のワンライナーに正規化する
///
/// Claude が生成する SQL には改行や連続スペースが含まれることがあるため、
/// 1行表示の入力エリアにセットする前にフラットな文字列に変換する。
/// 変換内容:
/// 1. `\r\n` / `\r` / `\n` をスペースに置換
/// 2. 連続するスペースを1つに圧縮
/// 3. 前後をトリム
fn normalize_sql_to_oneliner(sql: &str) -> String {
    // \r\n を先に処理することで \r が二重変換されるのを防ぐ
    let replaced = sql.replace("\r\n", " ").replace(['\r', '\n'], " ");
    // 連続スペースを1つに圧縮する
    replaced
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// SHOW TABLES を実行して結果を Vec<String> で返す
///
/// SHOW TABLES FROM <db> を使用することで USE 不要・専用コネクション不要のシンプルな実装にする。
/// USE → SHOW TABLES の2ステップ方式はセッション状態が不安定になることがあるため採用しない。
async fn fetch_tables(
    pool: &sqlx::Pool<sqlx::MySql>,
    database: Option<&str>,
) -> std::result::Result<Vec<String>, sqlx::Error> {
    use sqlx::Row;
    let rows = if let Some(db) = database {
        sqlx::query(&format!(
            "SHOW TABLES FROM {}",
            crate::query::escape_identifier(db)
        ))
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query("SHOW TABLES").fetch_all(pool).await?
    };
    Ok(rows
        .iter()
        .map(|row| row.try_get::<String, _>(0).unwrap_or_default())
        .collect())
}

/// 補完キャッシュを初期化する（接続確立直後に1回呼ぶ）
///
/// SHOW TABLES と SHOW DATABASES を並列取得してキャッシュに書き込む。
/// current_database が Some の場合、SHOW TABLES FROM <db> を使用することで
/// USE 不要・専用コネクション不要のシンプルな実装にする。
async fn initialize_completion_cache(
    cache: &Arc<tokio::sync::RwLock<CompletionCache>>,
    pool: &sqlx::Pool<sqlx::MySql>,
    current_database: Option<&str>,
) -> crate::error::Result<()> {
    use sqlx::Row;

    // SHOW DATABASES はDB切り替え不要なのでプール経由で取得する
    let databases_result = sqlx::query("SHOW DATABASES").fetch_all(pool).await;

    let tables = fetch_tables(pool, current_database)
        .await
        .map_err(crate::error::Error::QueryExecution)?;

    let databases: Vec<String> = databases_result
        .map_err(crate::error::Error::QueryExecution)?
        .iter()
        .map(|row| row.try_get::<String, _>(0).unwrap_or_default())
        .collect();

    let mut cache_write = cache.write().await;
    cache_write.tables = tables;
    cache_write.databases = databases;
    cache_write.is_ready = true;

    tracing::debug!(
        "Completion cache initialized: {} tables, {} databases",
        cache_write.tables.len(),
        cache_write.databases.len()
    );

    Ok(())
}

/// テーブルキャッシュを更新する（USE実行後に呼ぶ）
///
/// SHOW TABLES のみ再取得してキャッシュを更新する。
/// current_database が Some の場合、SHOW TABLES FROM <db> を使用することで
/// USE 不要・専用コネクション不要のシンプルな実装にする。
async fn refresh_table_cache(
    cache: &Arc<tokio::sync::RwLock<CompletionCache>>,
    pool: &sqlx::Pool<sqlx::MySql>,
    current_database: Option<&str>,
) -> crate::error::Result<()> {
    let tables = fetch_tables(pool, current_database)
        .await
        .map_err(crate::error::Error::QueryExecution)?;

    let mut cache_write = cache.write().await;
    cache_write.tables = tables;

    tracing::debug!("Table cache refreshed: {} tables", cache_write.tables.len());

    Ok(())
}
impl Drop for App {
    fn drop(&mut self) {
        self.abort_running_query();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    /// テスト用に sql.text のみをセットした最小限の App を生成する
    ///
    /// App::new() は Config 等の複雑な依存があるため、テストでは
    /// 必要なフィールドのみをセットした App を直接構築する。
    fn make_app_with_input(input: &str) -> App {
        App {
            state: AppState::Selecting {
                connections: Vec::new(),
                selected_index: 0,
            },
            should_quit: false,
            running_query: None,
            selected_record: None,
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            current_database: None,
            connection_name: None,
            bastion_name: None,
            current_table: None,
            readonly: false,
            input_focus: InputFocus::default(),
            sql: SqlInputState {
                text: input.to_string(),
                cursor_position: 0,
                selection_start: None,
                last_sql: String::new(),
                history: std::collections::VecDeque::new(),
                history_index: None,
                history_draft: String::new(),
                kill_buffer: String::new(),
                completion_cache: Arc::new(tokio::sync::RwLock::new(
                    crate::completion::CompletionCache::new(),
                )),
                completion_state: None,
            },
            shell: ShellInputState {
                text: String::new(),
                cursor_position: 0,
                selection_start: None,
                kill_buffer: String::new(),
                history: std::collections::VecDeque::new(),
                history_index: None,
                history_draft: String::new(),
                pending_command: None,
            },
            settings: crate::config::AppSettings::default(),
            prompt: PromptInputState {
                text: String::new(),
                cursor_position: 0,
                selection_start: None,
                kill_buffer: String::new(),
                is_processing: false,
                last_error: None,
                loading_tick: 0,
            },
            prompt_task: None,
        }
    }

    // ============================================================
    // タスク 10-12: InputFocus のデフォルト値と Shell 履歴のユニットテスト
    // ============================================================

    #[test]
    fn test_input_focus_default() {
        assert_eq!(InputFocus::default(), InputFocus::Sql);
    }

    #[test]
    fn test_shell_history_add_dedup() {
        let mut app = make_app_with_input("");
        app.add_to_shell_history("ls -la");
        app.add_to_shell_history("ls -la"); // 重複
        assert_eq!(app.shell.history.len(), 1);
    }

    #[test]
    fn test_shell_history_limit() {
        let mut app = make_app_with_input("");
        // MAX_SQL_HISTORY + 1 件追加すると最古が削除される
        for i in 0..=MAX_SQL_HISTORY {
            app.add_to_shell_history(&format!("cmd_{}", i));
        }
        assert_eq!(app.shell.history.len(), MAX_SQL_HISTORY);
        // 最古の "cmd_0" が削除されて "cmd_1" が先頭になるはず
        assert_eq!(app.shell.history.front().map(|s| s.as_str()), Some("cmd_1"));
    }

    #[test]
    fn test_shell_history_prev_next() {
        let mut app = make_app_with_input("");
        app.add_to_shell_history("echo hello");
        app.add_to_shell_history("ls");

        // ↑ で最新の "ls" を表示
        app.shell_history_prev();
        assert_eq!(app.shell.text, "ls");
        assert_eq!(app.shell.history_index, Some(1));

        // ↑ でさらに古い "echo hello" を表示
        app.shell_history_prev();
        assert_eq!(app.shell.text, "echo hello");
        assert_eq!(app.shell.history_index, Some(0));

        // ↓ で "ls" に戻る
        app.shell_history_next();
        assert_eq!(app.shell.text, "ls");
        assert_eq!(app.shell.history_index, Some(1));
    }

    #[test]
    fn test_shell_history_draft_restore() {
        let mut app = make_app_with_input("");
        app.shell.text = "draft text".to_string();
        app.add_to_shell_history("echo hello");

        // ↑ で履歴参照開始（draft は "draft text" として退避）
        app.shell_history_prev();
        assert_eq!(app.shell.text, "echo hello");
        assert_eq!(app.shell.history_draft, "draft text");

        // ↓ で末尾を超えると draft が復元される
        app.shell_history_next();
        assert_eq!(app.shell.text, "draft text");
        assert_eq!(app.shell.history_index, None);
    }

    // ============================================================
    // タスク 10-13: Tab フォーカス切り替えと Shell 入力のユニットテスト
    // ============================================================

    #[tokio::test]
    async fn test_tab_switches_focus_sql_to_shell() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Sql;

        let tab_event = KeyEvent {
            code: KeyCode::Tab,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(tab_event).await.unwrap();
        assert_eq!(app.input_focus, InputFocus::Shell);
    }

    #[tokio::test]
    async fn test_tab_switches_focus_shell_to_sql() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Shell;

        let tab_event = KeyEvent {
            code: KeyCode::Tab,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(tab_event).await.unwrap();
        assert_eq!(app.input_focus, InputFocus::Sql);
    }

    #[tokio::test]
    async fn test_tab_does_not_switch_focus_when_completion_visible() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Sql;
        // 補完ポップアップを表示状態にする
        app.sql.completion_state = Some(CompletionState {
            candidates: vec![crate::completion::CompletionItem {
                text: "SELECT".to_string(),
                kind: crate::completion::CompletionKind::Keyword,
            }],
            selected_index: 0,
            current_token: "S".to_string(),
        });

        let tab_event = KeyEvent {
            code: KeyCode::Tab,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(tab_event).await.unwrap();
        // 補完中は Tab でフォーカスが切り替わらない（補完が1つ進むだけ）
        assert_eq!(app.input_focus, InputFocus::Sql);
    }

    #[tokio::test]
    async fn test_shell_input_char_insert() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Shell;

        let char_event = KeyEvent {
            code: KeyCode::Char('l'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(char_event).await.unwrap();
        assert_eq!(app.shell.text, "l");
        assert_eq!(app.shell.cursor_position, 1);
    }

    #[tokio::test]
    async fn test_shell_input_backspace() {
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Shell;
        app.shell.text = "ls".to_string();
        app.shell.cursor_position = 2;

        let backspace_event = KeyEvent {
            code: KeyCode::Backspace,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(backspace_event).await.unwrap();
        assert_eq!(app.shell.text, "l");
        assert_eq!(app.shell.cursor_position, 1);
    }

    // ============================================================
    // Shell入力の選択・編集機能テスト
    // ============================================================

    #[tokio::test]
    async fn test_shell_shift_right_sets_selection() {
        // 選択なし状態で Shift+Right を押すと現在位置が anchor になり右に伸びることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Shell;
        app.shell.text = "ls -la".to_string();
        app.shell.cursor_position = 3;

        let ev = KeyEvent {
            code: KeyCode::Right,
            modifiers: KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.shell.selection_start, Some(3));
        assert_eq!(app.shell.cursor_position, 4);
    }

    #[tokio::test]
    async fn test_shell_shift_left_sets_selection() {
        // 選択なし状態で Shift+Left を押すと現在位置が anchor になり左に伸びることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Shell;
        app.shell.text = "ls -la".to_string();
        app.shell.cursor_position = 4;

        let ev = KeyEvent {
            code: KeyCode::Left,
            modifiers: KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.shell.selection_start, Some(4));
        assert_eq!(app.shell.cursor_position, 3);
    }

    #[tokio::test]
    async fn test_shell_shift_home_selects_to_beginning() {
        // Shift+Home でカーソル位置を anchor として行頭まで選択されることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Shell;
        app.shell.text = "ls -la".to_string();
        app.shell.cursor_position = 4;

        let ev = KeyEvent {
            code: KeyCode::Home,
            modifiers: KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.shell.selection_start, Some(4));
        assert_eq!(app.shell.cursor_position, 0);
    }

    #[tokio::test]
    async fn test_shell_shift_end_selects_to_end() {
        // Shift+End でカーソル位置を anchor として行末まで選択されることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Shell;
        app.shell.text = "ls -la".to_string();
        app.shell.cursor_position = 2;

        let ev = KeyEvent {
            code: KeyCode::End,
            modifiers: KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.shell.selection_start, Some(2));
        assert_eq!(app.shell.cursor_position, 6);
    }

    #[tokio::test]
    async fn test_shell_left_clears_selection() {
        // 選択中に通常の Left を押すと選択が解除されることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Shell;
        app.shell.text = "ls -la".to_string();
        app.shell.cursor_position = 4;
        app.shell.selection_start = Some(2);

        let ev = KeyEvent {
            code: KeyCode::Left,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.shell.selection_start, None);
        assert_eq!(app.shell.cursor_position, 3);
    }

    #[tokio::test]
    async fn test_shell_right_clears_selection() {
        // 選択中に通常の Right を押すと選択が解除されることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Shell;
        app.shell.text = "ls -la".to_string();
        app.shell.cursor_position = 2;
        app.shell.selection_start = Some(4);

        let ev = KeyEvent {
            code: KeyCode::Right,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.shell.selection_start, None);
        assert_eq!(app.shell.cursor_position, 3);
    }

    #[tokio::test]
    async fn test_shell_char_replaces_selection() {
        // 選択範囲がある状態で文字を入力すると選択範囲が置換されることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Shell;
        app.shell.text = "hello world".to_string();
        app.shell.cursor_position = 5;
        app.shell.selection_start = Some(0); // "hello" を選択

        let ev = KeyEvent {
            code: KeyCode::Char('X'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.shell.text, "X world");
        assert_eq!(app.shell.cursor_position, 1);
        assert_eq!(app.shell.selection_start, None);
    }

    #[tokio::test]
    async fn test_shell_backspace_deletes_selection() {
        // 選択範囲がある状態で Backspace を押すと選択範囲全体が削除されることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Shell;
        app.shell.text = "hello world".to_string();
        app.shell.cursor_position = 5;
        app.shell.selection_start = Some(0); // "hello" を選択

        let ev = KeyEvent {
            code: KeyCode::Backspace,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.shell.text, " world");
        assert_eq!(app.shell.cursor_position, 0);
        assert_eq!(app.shell.selection_start, None);
    }

    #[tokio::test]
    async fn test_shell_delete_deletes_selection() {
        // 選択範囲がある状態で Delete を押すと選択範囲全体が削除されることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Shell;
        // cursor(11)が末尾, selection_start(6)が'w'の手前 → "world" を選択
        app.shell.text = "hello world".to_string();
        app.shell.cursor_position = 11;
        app.shell.selection_start = Some(6);

        let ev = KeyEvent {
            code: KeyCode::Delete,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.shell.text, "hello ");
        assert_eq!(app.shell.cursor_position, 6);
        assert_eq!(app.shell.selection_start, None);
    }

    #[tokio::test]
    async fn test_shell_ctrl_k_kill_and_yank() {
        // Ctrl+K でカーソル以降が kill_buffer に保存され、Ctrl+Y で元の位置に挿入されることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Shell;
        app.shell.text = "ls -la".to_string();
        app.shell.cursor_position = 3;

        let ev_k = KeyEvent {
            code: KeyCode::Char('k'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev_k).await.unwrap();
        assert_eq!(app.shell.text, "ls ");
        assert_eq!(app.shell.cursor_position, 3);
        assert_eq!(app.shell.kill_buffer, "-la");

        // Ctrl+Y で kill_buffer の内容をペーストする
        let ev_y = KeyEvent {
            code: KeyCode::Char('y'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev_y).await.unwrap();
        assert_eq!(app.shell.text, "ls -la");
        assert_eq!(app.shell.cursor_position, 6);
    }

    #[tokio::test]
    async fn test_shell_ctrl_u_kills_before_cursor() {
        // Ctrl+U でカーソル以前のテキストが kill_buffer に保存されることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Shell;
        app.shell.text = "ls -la".to_string();
        app.shell.cursor_position = 3;

        let ev = KeyEvent {
            code: KeyCode::Char('u'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.shell.text, "-la");
        assert_eq!(app.shell.cursor_position, 0);
        assert_eq!(app.shell.kill_buffer, "ls ");
    }

    #[tokio::test]
    async fn test_shell_ctrl_w_deletes_word() {
        // Ctrl+W でカーソル直前の単語（word_left が返す範囲）が削除されることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Shell;
        // "hello world" の末尾から Ctrl+W → ' ' を超えて "world" が削除対象
        app.shell.text = "hello world".to_string();
        app.shell.cursor_position = 11;

        let ev = KeyEvent {
            code: KeyCode::Char('w'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.shell.text, "hello ");
        assert_eq!(app.shell.cursor_position, 6);
    }

    #[tokio::test]
    async fn test_shell_ctrl_a_selects_all() {
        // Ctrl+A で selection_start=Some(0)・cursor=末尾の全選択状態になることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Shell;
        app.shell.text = "ls -la".to_string();
        app.shell.cursor_position = 0;

        let ev = KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.shell.selection_start, Some(0));
        assert_eq!(app.shell.cursor_position, 6);
    }

    #[tokio::test]
    async fn test_shell_ctrl_e_clears_selection() {
        // Ctrl+E で選択が解除され、カーソルが行末に移動することを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Shell;
        app.shell.text = "ls -la".to_string();
        app.shell.cursor_position = 0;
        app.shell.selection_start = Some(3);

        let ev = KeyEvent {
            code: KeyCode::Char('e'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.shell.selection_start, None);
        assert_eq!(app.shell.cursor_position, 6);
    }

    #[tokio::test]
    async fn test_shell_multibyte_selection() {
        // マルチバイト文字を含む選択範囲の削除がバイト境界で正しく処理されることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Shell;
        // "echo "(5char) + "テスト"(3char) = 8char
        app.shell.text = "echo テスト".to_string();
        app.shell.cursor_position = 8;
        app.shell.selection_start = Some(5); // "テスト" を選択

        let ev = KeyEvent {
            code: KeyCode::Backspace,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.shell.text, "echo ");
        assert_eq!(app.shell.cursor_position, 5);
        assert_eq!(app.shell.selection_start, None);
    }

    // is_completion_separator のテスト

    #[test]
    fn test_is_word_separator() {
        // 区切り文字
        assert!(crate::completion::is_completion_separator(' '));
        assert!(crate::completion::is_completion_separator('\t'));
        assert!(crate::completion::is_completion_separator(','));
        assert!(crate::completion::is_completion_separator(';'));
        assert!(crate::completion::is_completion_separator('.'));
        assert!(crate::completion::is_completion_separator('('));
        assert!(crate::completion::is_completion_separator(')'));
        assert!(crate::completion::is_completion_separator('['));
        assert!(crate::completion::is_completion_separator(']'));
        assert!(crate::completion::is_completion_separator('='));
        assert!(crate::completion::is_completion_separator('<'));
        assert!(crate::completion::is_completion_separator('>'));
        assert!(crate::completion::is_completion_separator('!'));
        assert!(crate::completion::is_completion_separator('+'));
        assert!(crate::completion::is_completion_separator('-'));
        assert!(crate::completion::is_completion_separator('*'));
        assert!(crate::completion::is_completion_separator('/'));
        assert!(crate::completion::is_completion_separator('`'));
        assert!(crate::completion::is_completion_separator('\''));
        assert!(crate::completion::is_completion_separator('"'));
        // 単語文字
        assert!(!crate::completion::is_completion_separator('a'));
        assert!(!crate::completion::is_completion_separator('Z'));
        assert!(!crate::completion::is_completion_separator('0'));
        assert!(!crate::completion::is_completion_separator('9'));
        assert!(!crate::completion::is_completion_separator('_'));
        assert!(!crate::completion::is_completion_separator('あ')); // マルチバイト
        assert!(!crate::completion::is_completion_separator('テ'));
    }

    // word_left のテスト

    #[test]
    fn test_word_left_basic() {
        // "SELECT * FROM users"
        //  chars: S(0)E(1)L(2)E(3)C(4)T(5)' '(6)*(7)' '(8)F(9)R(10)O(11)M(12)' '(13)u(14)s(15)e(16)r(17)s(18)
        //  len = 19
        let app = make_app_with_input("SELECT * FROM users");

        // 末尾(19) → "users" の先頭(14)
        assert_eq!(app.word_left(19), 14);
        // "users" の先頭(14) → "FROM" の先頭(9): ' '(13)スキップ後 M(12)R(11)O(10)F(9) と遡る
        assert_eq!(app.word_left(14), 9);
        // "FROM" の先頭(9) → "SELECT" の先頭(0): ' '(8)と*(7)をスキップ後 T(5)..S(0) と遡る
        assert_eq!(app.word_left(9), 0);
    }

    #[test]
    fn test_word_left_from_zero() {
        let app = make_app_with_input("SELECT");
        // 位置0からの word_left は0を返す
        assert_eq!(app.word_left(0), 0);
    }

    #[test]
    fn test_word_left_multibyte() {
        // "SELECT * FROM テーブル WHERE id = 1"
        // テーブル は4文字(char単位)
        let input = "SELECT * FROM テーブル WHERE id = 1";
        let app = make_app_with_input(input);
        let len = input.chars().count();

        // 末尾から word_left を呼ぶと "1" をスキップして "=" の手前（空白スキップして id）に移動
        // "1" は1文字, " " は区切り, "=" は区切り, " " は区切り, "id" へ
        // 具体的な位置: "1"(末尾文字)の後ろ=len, 1文字前=len-1 は "1"(単語文字), その前は " "(区切り)
        // word_left(len) → "1" をスキップ → len-1 が "1" で単語文字, さらに前へ → " " は区切り → len-1
        assert_eq!(app.word_left(len), len - 1);

        // "テーブル" の末尾位置から word_left すると先頭("テ")に移動する
        // "SELECT * FROM " は 14文字 (0-13)、テーブルは chars[14..18]、末尾は18
        let table_end = 14 + 4; // 18
        let table_start = 14;
        assert_eq!(app.word_left(table_end), table_start);
    }

    #[test]
    fn test_word_left_consecutive_separators() {
        // 連続する区切り文字をスキップすること
        // "a  =  b" -> 末尾(7)から word_left → "b" をスキップして "a" の次へ
        let app = make_app_with_input("a  =  b");
        // 末尾(7) → b(6) は単語文字, その前 5..2 は区切り, a(0) は単語文字 → 0
        assert_eq!(app.word_left(7), 6);
        // b の手前(6) → 5,4,3 は区切り文字("  =") → a(0..1) を遡る → 0
        assert_eq!(app.word_left(6), 0);
    }

    // word_right のテスト

    #[test]
    fn test_word_right_basic() {
        // "SELECT * FROM users"
        //  chars: S(0)E(1)L(2)E(3)C(4)T(5)' '(6)*(7)' '(8)F(9)R(10)O(11)M(12)' '(13)u(14)s(15)e(16)r(17)s(18)
        //  len = 19
        let app = make_app_with_input("SELECT * FROM users");

        // 先頭(0) → "SELECT" の末尾(6): T(5) の次 = 6
        assert_eq!(app.word_right(0), 6);
        // (6) → ' '(6)・*(7)・' '(8) をスキップ後 F(9)R(10)O(11)M(12) と進む → 13
        assert_eq!(app.word_right(6), 13);
        // (13) → ' '(13) スキップ後 u(14)..s(18) と進む → 19
        assert_eq!(app.word_right(13), 19);
    }

    #[test]
    fn test_word_right_from_end() {
        let app = make_app_with_input("SELECT");
        let len = "SELECT".chars().count(); // 6
                                            // 末尾からの word_right は末尾のまま
        assert_eq!(app.word_right(len), len);
    }

    #[test]
    fn test_word_right_multibyte() {
        // "SELECT テーブル WHERE"
        // "SELECT "(7文字) + "テーブル"(4文字) + " WHERE"(6文字)
        let input = "SELECT テーブル WHERE";
        let app = make_app_with_input(input);

        // 先頭(0) → "SELECT" の末尾(6)
        assert_eq!(app.word_right(0), 6);
        // "SELECT" の末尾(6) → "テーブル" の末尾(11): 空白スキップ後にテーブルを進む
        // " "(6)はスキップ, テ(7)ー(8)ブ(9)ル(10)=末尾11
        assert_eq!(app.word_right(6), 11);
        // "テーブル" の末尾(11) → "WHERE" の末尾(17)
        assert_eq!(app.word_right(11), 17);
    }

    #[test]
    fn test_word_right_consecutive_separators() {
        // 連続する区切り文字をスキップすること
        // "a  =  b": a(0), ' '(1), ' '(2), '='(3), ' '(4), ' '(5), b(6)
        let app = make_app_with_input("a  =  b");
        // 先頭(0) → a(0)は単語文字, 次(1)は区切り → a の末尾(1)
        assert_eq!(app.word_right(0), 1);
        // (1) → 1,2,3,4,5 は区切り文字 → b(6) を進む → 7
        assert_eq!(app.word_right(1), 7);
    }

    // ============================================================
    // prompt_word_left / prompt_word_right のユニットテスト
    // ============================================================

    #[test]
    fn test_prompt_word_left_basic() {
        // SQL の word_left と同じ文字列で同等の動作をすることを確認する
        // "SELECT * FROM users"
        //  chars: S(0)..T(5)' '(6)*(7)' '(8)F(9)..M(12)' '(13)u(14)..s(18)
        let mut app = make_app_with_input("");
        app.prompt.text = "SELECT * FROM users".to_string();

        assert_eq!(app.prompt_word_left(19), 14); // 末尾(19) → "users" の先頭(14)
        assert_eq!(app.prompt_word_left(14), 9); // "users"先頭(14) → "FROM" の先頭(9)
        assert_eq!(app.prompt_word_left(9), 0); // "FROM"先頭(9) → "SELECT" の先頭(0)
    }

    #[test]
    fn test_prompt_word_left_from_zero() {
        // 位置 0 からの prompt_word_left は 0 を返すことを確認する
        let mut app = make_app_with_input("");
        app.prompt.text = "SELECT".to_string();
        assert_eq!(app.prompt_word_left(0), 0);
    }

    #[test]
    fn test_prompt_word_left_multibyte() {
        // マルチバイト文字を含むテキストで char 単位の移動が正しいことを確認する
        // "SELECT * FROM テーブル WHERE id = 1"
        // "テーブル" は4文字(char単位)、先頭位置は14
        let input = "SELECT * FROM テーブル WHERE id = 1";
        let mut app = make_app_with_input("");
        app.prompt.text = input.to_string();
        let len = input.chars().count();

        // 末尾(len) → "1" の先頭(len-1)
        assert_eq!(app.prompt_word_left(len), len - 1);
        // "テーブル" の末尾(18) → 先頭(14)
        let table_end = 14 + 4; // 18
        assert_eq!(app.prompt_word_left(table_end), 14);
    }

    #[test]
    fn test_prompt_word_left_consecutive_separators() {
        // 連続する区切り文字をまとめてスキップすることを確認する
        let mut app = make_app_with_input("");
        app.prompt.text = "a  =  b".to_string();
        // 末尾(7) → 'b' をスキップ → '=' と空白をスキップ → 'a' の次(1)
        assert_eq!(app.prompt_word_left(7), 6);
        assert_eq!(app.prompt_word_left(6), 0);
    }

    #[test]
    fn test_prompt_word_right_basic() {
        // SQL の word_right と同じ文字列で同等の動作をすることを確認する
        let mut app = make_app_with_input("");
        app.prompt.text = "SELECT * FROM users".to_string();

        assert_eq!(app.prompt_word_right(0), 6); // 先頭(0) → "SELECT" の末尾(6)
        assert_eq!(app.prompt_word_right(6), 13); // (6) → "FROM" の末尾(13)
        assert_eq!(app.prompt_word_right(13), 19); // (13) → "users" の末尾(19)
    }

    #[test]
    fn test_prompt_word_right_from_end() {
        // 末尾からの prompt_word_right は末尾のまま返すことを確認する
        let mut app = make_app_with_input("");
        app.prompt.text = "SELECT".to_string();
        let len = "SELECT".chars().count();
        assert_eq!(app.prompt_word_right(len), len);
    }

    #[test]
    fn test_prompt_word_right_multibyte() {
        // マルチバイト文字を含むテキストで char 単位の移動が正しいことを確認する
        // "SELECT テーブル WHERE": "SELECT "(7) + "テーブル"(4) + " WHERE"(6)
        let input = "SELECT テーブル WHERE";
        let mut app = make_app_with_input("");
        app.prompt.text = input.to_string();

        assert_eq!(app.prompt_word_right(0), 6); // "SELECT" 末尾(6)
        assert_eq!(app.prompt_word_right(6), 11); // "テーブル" 末尾(11): ' '(6)スキップ後テ(7)ー(8)ブ(9)ル(10)
        assert_eq!(app.prompt_word_right(11), 17); // "WHERE" 末尾(17)
    }

    #[test]
    fn test_prompt_word_right_consecutive_separators() {
        // 連続する区切り文字をまとめてスキップすることを確認する
        let mut app = make_app_with_input("");
        app.prompt.text = "a  =  b".to_string();
        assert_eq!(app.prompt_word_right(0), 1); // 先頭(0) → 'a'末尾(1)
        assert_eq!(app.prompt_word_right(1), 7); // (1) → 区切り文字群スキップして 'b'末尾(7)
    }

    // ============================================================
    // Prompt入力の選択・編集機能テスト
    // ============================================================

    #[tokio::test]
    async fn test_prompt_shift_right_sets_selection() {
        // 選択なし状態で Shift+Right を押すと現在位置が anchor になり右に伸びることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Prompt;
        app.prompt.text = "hello world".to_string();
        app.prompt.cursor_position = 3;

        let ev = KeyEvent {
            code: KeyCode::Right,
            modifiers: KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.prompt.selection_start, Some(3));
        assert_eq!(app.prompt.cursor_position, 4);
    }

    #[tokio::test]
    async fn test_prompt_shift_left_sets_selection() {
        // 選択なし状態で Shift+Left を押すと現在位置が anchor になり左に伸びることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Prompt;
        app.prompt.text = "hello world".to_string();
        app.prompt.cursor_position = 5;

        let ev = KeyEvent {
            code: KeyCode::Left,
            modifiers: KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.prompt.selection_start, Some(5));
        assert_eq!(app.prompt.cursor_position, 4);
    }

    #[tokio::test]
    async fn test_prompt_shift_home_selects_to_beginning() {
        // Shift+Home でカーソル位置を anchor として行頭まで選択されることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Prompt;
        app.prompt.text = "hello world".to_string();
        app.prompt.cursor_position = 5;

        let ev = KeyEvent {
            code: KeyCode::Home,
            modifiers: KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.prompt.selection_start, Some(5));
        assert_eq!(app.prompt.cursor_position, 0);
    }

    #[tokio::test]
    async fn test_prompt_shift_end_selects_to_end() {
        // Shift+End でカーソル位置を anchor として行末まで選択されることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Prompt;
        app.prompt.text = "hello world".to_string();
        app.prompt.cursor_position = 3;

        let ev = KeyEvent {
            code: KeyCode::End,
            modifiers: KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.prompt.selection_start, Some(3));
        assert_eq!(app.prompt.cursor_position, 11);
    }

    #[tokio::test]
    async fn test_prompt_left_clears_selection() {
        // 選択中に通常の Left を押すと選択が解除されることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Prompt;
        app.prompt.text = "hello world".to_string();
        app.prompt.cursor_position = 5;
        app.prompt.selection_start = Some(2);

        let ev = KeyEvent {
            code: KeyCode::Left,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.prompt.selection_start, None);
        assert_eq!(app.prompt.cursor_position, 4);
    }

    #[tokio::test]
    async fn test_prompt_right_clears_selection() {
        // 選択中に通常の Right を押すと選択が解除されることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Prompt;
        app.prompt.text = "hello world".to_string();
        app.prompt.cursor_position = 3;
        app.prompt.selection_start = Some(8);

        let ev = KeyEvent {
            code: KeyCode::Right,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.prompt.selection_start, None);
        assert_eq!(app.prompt.cursor_position, 4);
    }

    #[tokio::test]
    async fn test_prompt_char_replaces_selection() {
        // 選択範囲がある状態で文字を入力すると選択範囲が置換されることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Prompt;
        app.prompt.text = "hello world".to_string();
        app.prompt.cursor_position = 5;
        app.prompt.selection_start = Some(0); // "hello" を選択

        let ev = KeyEvent {
            code: KeyCode::Char('Y'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.prompt.text, "Y world");
        assert_eq!(app.prompt.cursor_position, 1);
        assert_eq!(app.prompt.selection_start, None);
    }

    #[tokio::test]
    async fn test_prompt_backspace_deletes_selection() {
        // 選択範囲がある状態で Backspace を押すと選択範囲全体が削除されることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Prompt;
        app.prompt.text = "hello world".to_string();
        app.prompt.cursor_position = 5;
        app.prompt.selection_start = Some(0); // "hello" を選択

        let ev = KeyEvent {
            code: KeyCode::Backspace,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.prompt.text, " world");
        assert_eq!(app.prompt.cursor_position, 0);
        assert_eq!(app.prompt.selection_start, None);
    }

    #[tokio::test]
    async fn test_prompt_delete_deletes_selection() {
        // 選択範囲がある状態で Delete を押すと選択範囲全体が削除されることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Prompt;
        // cursor(11)が末尾, selection_start(6)が'w'の手前 → "world" を選択
        app.prompt.text = "hello world".to_string();
        app.prompt.cursor_position = 11;
        app.prompt.selection_start = Some(6);

        let ev = KeyEvent {
            code: KeyCode::Delete,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.prompt.text, "hello ");
        assert_eq!(app.prompt.cursor_position, 6);
        assert_eq!(app.prompt.selection_start, None);
    }

    #[tokio::test]
    async fn test_prompt_ctrl_k_kill_and_yank() {
        // Ctrl+K でカーソル以降が kill_buffer に保存され、Ctrl+Y で元の位置に挿入されることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Prompt;
        app.prompt.text = "hello world".to_string();
        app.prompt.cursor_position = 6;

        let ev_k = KeyEvent {
            code: KeyCode::Char('k'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev_k).await.unwrap();
        assert_eq!(app.prompt.text, "hello ");
        assert_eq!(app.prompt.cursor_position, 6);
        assert_eq!(app.prompt.kill_buffer, "world");

        // Ctrl+Y で kill_buffer の内容をペーストする
        let ev_y = KeyEvent {
            code: KeyCode::Char('y'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev_y).await.unwrap();
        assert_eq!(app.prompt.text, "hello world");
        assert_eq!(app.prompt.cursor_position, 11);
    }

    #[tokio::test]
    async fn test_prompt_ctrl_u_kills_before_cursor() {
        // Ctrl+U でカーソル以前のテキストが kill_buffer に保存されることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Prompt;
        app.prompt.text = "hello world".to_string();
        app.prompt.cursor_position = 6;

        let ev = KeyEvent {
            code: KeyCode::Char('u'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.prompt.text, "world");
        assert_eq!(app.prompt.cursor_position, 0);
        assert_eq!(app.prompt.kill_buffer, "hello ");
    }

    #[tokio::test]
    async fn test_prompt_ctrl_w_deletes_word() {
        // Ctrl+W でカーソル直前の単語（word_left が返す範囲）が削除されることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Prompt;
        // "hello world" の末尾から Ctrl+W → "world" が削除対象
        app.prompt.text = "hello world".to_string();
        app.prompt.cursor_position = 11;

        let ev = KeyEvent {
            code: KeyCode::Char('w'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.prompt.text, "hello ");
        assert_eq!(app.prompt.cursor_position, 6);
    }

    #[tokio::test]
    async fn test_prompt_ctrl_a_selects_all() {
        // Ctrl+A で selection_start=Some(0)・cursor=末尾の全選択状態になることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Prompt;
        app.prompt.text = "hello world".to_string();
        app.prompt.cursor_position = 0;

        let ev = KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.prompt.selection_start, Some(0));
        assert_eq!(app.prompt.cursor_position, 11);
    }

    #[tokio::test]
    async fn test_prompt_ctrl_e_clears_selection() {
        // Ctrl+E で選択が解除され、カーソルが行末に移動することを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Prompt;
        app.prompt.text = "hello world".to_string();
        app.prompt.cursor_position = 0;
        app.prompt.selection_start = Some(5);

        let ev = KeyEvent {
            code: KeyCode::Char('e'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.prompt.selection_start, None);
        assert_eq!(app.prompt.cursor_position, 11);
    }

    #[tokio::test]
    async fn test_prompt_multibyte_selection() {
        // マルチバイト文字を含む選択範囲の削除がバイト境界で正しく処理されることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Prompt;
        // "SQL: "(5char) + "テーブル名を教えて"(9char) = 14char
        app.prompt.text = "SQL: テーブル名を教えて".to_string();
        app.prompt.cursor_position = 14;
        app.prompt.selection_start = Some(5); // "テーブル名を教えて" を選択

        let ev = KeyEvent {
            code: KeyCode::Backspace,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        assert_eq!(app.prompt.text, "SQL: ");
        assert_eq!(app.prompt.cursor_position, 5);
        assert_eq!(app.prompt.selection_start, None);
    }

    #[tokio::test]
    async fn test_prompt_editing_ignored_while_processing() {
        // is_processing が true の間は編集系キー入力が無視されることを確認する
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let mut app = make_app_with_input("");
        app.input_focus = InputFocus::Prompt;
        app.prompt.text = "hello".to_string();
        app.prompt.cursor_position = 5;
        app.prompt.is_processing = true; // 処理中フラグを立てる

        let ev = KeyEvent {
            code: KeyCode::Char('X'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_connected_input(ev).await.unwrap();
        // 処理中のため文字が挿入されていないことを確認する
        assert_eq!(app.prompt.text, "hello");
        assert_eq!(app.prompt.cursor_position, 5);
    }

    // ============================================================
    // is_write_sql / first_meaningful_token のユニットテスト
    // ============================================================

    #[test]
    fn test_is_write_sql_with_comments() {
        // ブロックコメント付きINSERT
        assert!(is_write_sql("/* comment */ INSERT INTO users VALUES (1)"));
        // 行コメント付きDELETE
        assert!(is_write_sql("-- delete old data\nDELETE FROM users"));
        // 複数コメント付きUPDATE
        assert!(is_write_sql("/* a */ -- b\nUPDATE users SET name = 'x'"));
        // コメント付きSELECTはfalse
        assert!(!is_write_sql("/* hint */ SELECT * FROM users"));
        // コメントのみ
        assert!(!is_write_sql("/* only comment */"));
    }

    #[test]
    fn test_is_write_sql_basic() {
        // コメントなし通常ケースで既存の動作が維持されること
        assert!(is_write_sql("INSERT INTO users VALUES (1)"));
        assert!(is_write_sql("UPDATE users SET name = 'x'"));
        assert!(is_write_sql("DELETE FROM users"));
        assert!(is_write_sql("DROP TABLE users"));
        assert!(is_write_sql("ALTER TABLE users ADD COLUMN age INT"));
        assert!(is_write_sql("TRUNCATE TABLE users"));
        assert!(is_write_sql("CREATE TABLE foo (id INT)"));
        assert!(is_write_sql("REPLACE INTO users VALUES (1)"));
        assert!(is_write_sql("RENAME TABLE a TO b"));
        assert!(is_write_sql("GRANT ALL ON db.* TO 'user'@'%'"));
        assert!(is_write_sql("REVOKE ALL ON db.* FROM 'user'@'%'"));
        // 読み取り系はfalse
        assert!(!is_write_sql("SELECT * FROM users"));
        assert!(!is_write_sql("SHOW TABLES"));
        assert!(!is_write_sql("USE mydb"));
        // 大文字小文字混在
        assert!(is_write_sql("insert into users values (1)"));
        assert!(is_write_sql("Insert Into users values (1)"));
    }

    #[test]
    fn test_first_meaningful_token_edge_cases() {
        // 空文字列
        assert_eq!(first_meaningful_token(""), "");
        // 空白のみ
        assert_eq!(first_meaningful_token("   "), "");
        // 閉じないブロックコメント
        assert_eq!(first_meaningful_token("/* unclosed"), "");
        // 改行のない行コメント（ファイル末尾）
        assert_eq!(first_meaningful_token("-- trailing comment"), "");
        // ネストしないブロックコメント後にトークン
        assert_eq!(first_meaningful_token("/* c1 */ /* c2 */ SELECT"), "SELECT");
    }

    #[test]
    fn test_is_write_sql_with_semicolons() {
        // セミコロン付きSQL
        assert!(is_write_sql("DELETE FROM users;"));
        assert!(is_write_sql("INSERT INTO users VALUES (1);"));
        assert!(!is_write_sql("SELECT * FROM users;"));
    }

    #[test]
    fn test_is_write_sql_cte_prefixed() {
        // CTE + SELECT（読み取り）
        assert!(!is_write_sql("WITH cte AS (SELECT 1) SELECT * FROM cte"));
        // CTE + DELETE（書き込み）
        assert!(is_write_sql("WITH old AS (SELECT id FROM users WHERE age > 100) DELETE FROM users WHERE id IN (SELECT id FROM old)"));
        // CTE + INSERT（書き込み）
        assert!(is_write_sql(
            "WITH src AS (SELECT * FROM temp) INSERT INTO users SELECT * FROM src"
        ));
        // CTE + UPDATE（書き込み）
        assert!(is_write_sql("WITH targets AS (SELECT id FROM users) UPDATE users SET active = 0 WHERE id IN (SELECT id FROM targets)"));
        // 小文字CTE + 書き込み
        assert!(is_write_sql("with cte as (select 1) delete from users"));
        // 小文字CTE + 読み取り
        assert!(!is_write_sql("with cte as (select 1) select * from cte"));
    }

    #[test]
    fn test_is_write_sql_lowercase_and_mixed_case() {
        assert!(is_write_sql("insert into users values (1)"));
        assert!(is_write_sql("dElEtE FROM users"));
        assert!(is_write_sql("  update users set x = 1"));
        assert!(!is_write_sql("select 1"));
        assert!(!is_write_sql("  show tables"));
    }

    // ============================================================
    // update_current_database のユニットテスト
    // ============================================================

    #[test]
    fn test_update_current_database_backtick_with_semicolon() {
        // バグ修正の検証: `foo`; のセミコロン除去後にバッククォートが残らないこと
        let mut app = make_app_with_input("");
        app.sql.last_sql = "USE `foo`;".to_string();
        app.update_current_database();
        assert_eq!(app.current_database, Some("foo".to_string()));
    }

    #[test]
    fn test_update_current_database_no_backtick() {
        // バッククォートなし・セミコロンなし
        let mut app = make_app_with_input("");
        app.sql.last_sql = "USE mydb".to_string();
        app.update_current_database();
        assert_eq!(app.current_database, Some("mydb".to_string()));
    }

    #[test]
    fn test_update_current_database_with_semicolon_no_backtick() {
        // バッククォートなし・セミコロンあり
        let mut app = make_app_with_input("");
        app.sql.last_sql = "USE mydb;".to_string();
        app.update_current_database();
        assert_eq!(app.current_database, Some("mydb".to_string()));
    }

    #[test]
    fn test_update_current_database_backtick_no_semicolon() {
        // バッククォートあり・セミコロンなし（ハイフン等を含むDB名）
        let mut app = make_app_with_input("");
        app.sql.last_sql = "USE `my-db`".to_string();
        app.update_current_database();
        assert_eq!(app.current_database, Some("my-db".to_string()));
    }

    // ============================================================
    // read_row_from_chunk のユニットテスト
    // ============================================================

    /// チャンクファイルに1行書き込んで read_row_from_chunk で正しく読み出せることを確認する
    #[test]
    fn test_read_row_from_chunk_single_row() {
        let dir = tempfile::tempdir().unwrap();
        let columns = vec!["id".to_string(), "name".to_string(), "value".to_string()];
        let data = vec!["1".to_string(), "Alice".to_string(), "100".to_string()];

        // append_preview_to_chunk でチャンクバッファに書き込む
        let mut chunk_buf = String::new();
        append_preview_to_chunk(dir.path(), 0, &columns, &data, &mut chunk_buf);
        // PREVIEW_CHUNK_SIZE 未満なのでバッファのまま、手動で flush する
        flush_preview_chunk(dir.path(), 0, &chunk_buf);

        let result = read_row_from_chunk(dir.path(), 0, &columns).unwrap();
        assert_eq!(result, data);
    }

    /// 複数行を書き込んで任意の行を正しく読み出せることを確認する
    #[test]
    fn test_read_row_from_chunk_multiple_rows() {
        let dir = tempfile::tempdir().unwrap();
        let columns = vec!["col1".to_string(), "col2".to_string()];

        let rows: Vec<Vec<String>> = (0..5)
            .map(|i| vec![format!("val1_{}", i), format!("val2_{}", i)])
            .collect();

        let mut chunk_buf = String::new();
        for (i, row) in rows.iter().enumerate() {
            append_preview_to_chunk(dir.path(), i, &columns, row, &mut chunk_buf);
        }
        flush_preview_chunk(dir.path(), rows.len() - 1, &chunk_buf);

        // 先頭行・中間行・末尾行を正しく読み出せること
        assert_eq!(
            read_row_from_chunk(dir.path(), 0, &columns).unwrap(),
            rows[0]
        );
        assert_eq!(
            read_row_from_chunk(dir.path(), 2, &columns).unwrap(),
            rows[2]
        );
        assert_eq!(
            read_row_from_chunk(dir.path(), 4, &columns).unwrap(),
            rows[4]
        );
    }

    /// PREVIEW_CHUNK_SIZE 境界をまたぐ行のアクセスが正しく動作することを確認する
    #[test]
    fn test_read_row_from_chunk_across_chunk_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let columns = vec!["id".to_string()];
        let total_rows = PREVIEW_CHUNK_SIZE + 3;

        let mut chunk_buf = String::new();
        for i in 0..total_rows {
            let data = vec![i.to_string()];
            append_preview_to_chunk(dir.path(), i, &columns, &data, &mut chunk_buf);
        }
        flush_preview_chunk(dir.path(), total_rows - 1, &chunk_buf);

        // チャンク0の最後の行
        let last_in_chunk0 = PREVIEW_CHUNK_SIZE - 1;
        let result0 = read_row_from_chunk(dir.path(), last_in_chunk0, &columns).unwrap();
        assert_eq!(result0, vec![last_in_chunk0.to_string()]);

        // チャンク1の最初の行
        let first_in_chunk1 = PREVIEW_CHUNK_SIZE;
        let result1 = read_row_from_chunk(dir.path(), first_in_chunk1, &columns).unwrap();
        assert_eq!(result1, vec![first_in_chunk1.to_string()]);

        // チャンク1の3行目
        let third_in_chunk1 = PREVIEW_CHUNK_SIZE + 2;
        let result2 = read_row_from_chunk(dir.path(), third_in_chunk1, &columns).unwrap();
        assert_eq!(result2, vec![third_in_chunk1.to_string()]);
    }

    /// カラム名に ": " が含まれる場合でもフォールバックで値を取得できることを確認する
    #[test]
    fn test_read_row_from_chunk_column_with_colon() {
        let dir = tempfile::tempdir().unwrap();
        // カラム名にコロンを含む場合: フォールバックロジックを使う
        let columns = vec!["col:name".to_string(), "normal".to_string()];
        let data = vec!["value1".to_string(), "value2".to_string()];

        let mut chunk_buf = String::new();
        append_preview_to_chunk(dir.path(), 0, &columns, &data, &mut chunk_buf);
        flush_preview_chunk(dir.path(), 0, &chunk_buf);

        let result = read_row_from_chunk(dir.path(), 0, &columns).unwrap();
        // フォールバック: ": " の最初の位置で分割するため "value1" が取得できる
        assert_eq!(result[0], "value1");
        assert_eq!(result[1], "value2");
    }

    /// 存在しないチャンクファイルへのアクセスはエラーを返すことを確認する
    #[test]
    fn test_read_row_from_chunk_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let columns = vec!["id".to_string()];

        let result = read_row_from_chunk(dir.path(), 0, &columns);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("チャンクファイルの読み込みに失敗しました"));
    }

    /// 存在する行インデックスを超えた場合はエラーを返すことを確認する
    #[test]
    fn test_read_row_from_chunk_out_of_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let columns = vec!["id".to_string()];
        let data = vec!["1".to_string()];

        let mut chunk_buf = String::new();
        append_preview_to_chunk(dir.path(), 0, &columns, &data, &mut chunk_buf);
        flush_preview_chunk(dir.path(), 0, &chunk_buf);

        // 同じチャンク内の存在しない行（インデックス1）を読み出そうとするとエラー
        let result = read_row_from_chunk(dir.path(), 1, &columns);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("選択された行がチャンクファイル内に見つかりません"));
    }
}
