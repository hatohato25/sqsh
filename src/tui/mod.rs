use crossterm::{
    event::{self},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    Terminal,
};
use crate::completion::{CompletionCache, CompletionItem};
use ::skim::prelude::*;
use std::borrow::Cow;
use std::collections::VecDeque;
use unicode_width::UnicodeWidthStr;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::task::JoinHandle;

use crate::config::Config;
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
pub(super) fn is_write_sql(sql: &str) -> bool {
    let first_token = sql.split_whitespace().next().unwrap_or("").to_uppercase();
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
            tracing::warn!("プレビューチャンクの書き込みに失敗しました (chunk={}): {}", chunk_idx, e);
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
            tracing::warn!("最終プレビューチャンクの書き込みに失敗しました (chunk={}): {}", chunk_idx, e);
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
        .map_err(|e| {
            crate::error::Error::Other(format!("{}: {:?}", t!(TuiMsg::SkimInitError), e))
        })
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
        SkimAction::DrillDown(format!("USE {}", crate::query::escape_identifier(first_value)))
    } else if first_column.starts_with("Tables_in_") {
        SkimAction::DrillDown(format!("SELECT * FROM {}", crate::query::escape_identifier(first_value)))
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
        // extract_from_table は "db.table" 形式（バッククォートなし）を返すため
        // "." で分割して各部分を個別に escape_identifier に渡す
        let table_raw = crate::completion::extract_from_table(source_sql).unwrap_or_else(|| "?".to_string());
        let escaped_table = if let Some((db, tbl)) = table_raw.split_once('.') {
            format!(
                "{}.{}",
                crate::query::escape_identifier(db),
                crate::query::escape_identifier(tbl)
            )
        } else {
            crate::query::escape_identifier(&table_raw)
        };
        // MySQLのエスケープルールに従い、バックスラッシュ→シングルクォートの順に処理する
        // 順序が重要: バックスラッシュを先にエスケープしないと、後のシングルクォートエスケープが壊れる
        let escaped_value = first_value.replace('\\', "\\\\").replace('\'', "\\'");
        let where_clause = format!(
            "SELECT * FROM {} WHERE {} = '{}'",
            escaped_table,
            crate::query::escape_identifier(first_column),
            escaped_value
        );
        SkimAction::SelectRecord {
            where_template: where_clause,
            record,
        }
    }
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

    /// 接続処理中（バックグラウンドで接続を試みている間）
    Connecting {
        connection_name: String,
        /// スピナーアニメーションのフレーム番号
        spinner_frame: u8,
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

    /// 入力中のSQLクエリ（Connected状態で使用）
    pub(super) query_input: String,

    /// 終了フラグ
    pub(super) should_quit: bool,

    /// バックグラウンドで実行中のクエリ
    pub(super) running_query: Option<RunningQuery>,

    /// 選択されたレコードのプレビュー情報（SQL入力画面で表示）
    pub(super) selected_record: Option<SelectedRecord>,

    /// グレースフルシャットダウン用フラグ
    pub(super) shutdown_flag: Arc<AtomicBool>,

    /// SQL入力欄のカーソル位置（char単位）
    ///
    /// query_inputはUTF-8文字列なので、バイト位置ではなくchar単位で管理する。
    /// 描画時やBackspace/insert時にchar_indices()でバイト位置へ変換して使用する。
    pub(super) cursor_position: usize,

    /// SQL入力欄のテキスト選択開始位置（char単位、None=選択なし）
    ///
    /// Shift+矢印キーで選択範囲を設定する。cursor_positionと組み合わせて
    /// min(selection_start, cursor_position)..max(selection_start, cursor_position) が選択範囲となる。
    pub(super) selection_start: Option<usize>,

    /// 最後に実行したSQLクエリ（WHEREテンプレート生成時にテーブル名を抽出するために保持）
    ///
    /// ShowingResult 遷移時に query_input がクリアされるため、
    /// show_result_with_skim でテーブル名を参照できるよう別途保存する。
    pub(super) last_sql: String,

    /// SQL実行履歴（最新が末尾）
    ///
    /// Enter実行時に追加し、直前と同じクエリは重複追加しない。
    /// MAX_SQL_HISTORY を超えた場合は先頭（最古）を削除する。
    /// 先頭削除がO(n)になるVecの代わりにVecDequeを使用する。
    pub(super) sql_history: VecDeque<String>,

    /// 履歴参照中の現在位置（None=新規入力中、Some(index)=履歴参照中）
    pub(super) history_index: Option<usize>,

    /// 履歴参照を開始した時点で退避しておいた入力中テキスト
    ///
    /// ↓キーで履歴末尾を超えて新規入力状態に戻る際に復元する。
    pub(super) history_draft: String,

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

    /// Ctrl+K / Ctrl+U で削除したテキストを保存するキルバッファ
    ///
    /// Ctrl+Y（yank）でペースト可能。システムクリップボードとは独立している。
    pub(super) kill_buffer: String,

    /// 補完候補キャッシュ（接続確立後に非同期で充填）
    ///
    /// Arc<tokio::sync::RwLock<...>> でラップし、バックグラウンドタスクから
    /// 書き込み、TUIの描画ループから読み取りを安全に行う。
    pub(super) completion_cache: Arc<tokio::sync::RwLock<CompletionCache>>,

    /// 補完ポップアップ状態
    ///
    /// None = ポップアップ非表示、Some(...) = 候補リスト表示中
    pub(super) completion_state: Option<CompletionState>,

    /// 全接続設定リスト（Selecting状態復帰時に使用）
    pub(super) connections: Vec<crate::config::ConnectionConfig>,

    /// 接続中のバックグラウンドタスク（Ctrl+C で abort するために保持）
    pub(super) connecting_task: Option<JoinHandle<crate::error::Result<ConnectionManager>>>,
}

impl App {
    /// 新しいアプリケーションを作成
    pub fn new(config: Config, shutdown_flag: Arc<AtomicBool>, cli_readonly: bool) -> Self {
        // default_bastionを適用した接続設定リストを取得
        let connections = config.resolve_connections();
        Self {
            state: AppState::Selecting {
                connections: connections.clone(),
                selected_index: 0,
            },
            query_input: String::new(),
            should_quit: false,
            running_query: None,
            selected_record: None,
            shutdown_flag,
            cursor_position: 0,
            selection_start: None,
            last_sql: String::new(),
            sql_history: VecDeque::new(),
            history_index: None,
            history_draft: String::new(),
            current_database: None,
            connection_name: None,
            current_table: None,
            readonly: cli_readonly,
            kill_buffer: String::new(),
            completion_cache: Arc::new(tokio::sync::RwLock::new(CompletionCache::new())),
            completion_state: None,
            connections,
            connecting_task: None,
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
            let connection_name = selected_connection.name.clone();
            self.connection_name = Some(connection_name.clone());

            tracing::info!("Connecting to: {}", connection_name);

            // 接続処理をバックグラウンドタスクに回してTUIループを先に起動し、接続中UIを表示できるようにする
            self.connecting_task = Some(tokio::spawn(async move {
                crate::connection::ConnectionManager::connect(selected_connection, readonly).await
            }));

            self.state = AppState::Connecting {
                connection_name,
                spinner_frame: 0,
            };

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

        // spawn_blocking 内の同期タスク（SSH/DNS）は abort() できないため、
        // 接続中にキャンセルされた場合はプロセスを即終了してタイムアウト待ちを回避する
        if self.connecting_task.is_some() {
            std::process::exit(0);
        }

        result
    }

    /// メインイベントループ
    async fn run_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<()> {
        loop {
            self.poll_query_completion().await?;
            self.poll_connecting().await?;

            // StreamingQuery状態に遷移した場合、ストリーミングでskimに渡す
            if matches!(self.state, AppState::StreamingQuery { .. }) {
                let (manager, sql, timeout_secs) = match std::mem::replace(
                    &mut self.state,
                    AppState::Selecting {
                        connections: Vec::new(),
                        selected_index: 0,
                    },
                ) {
                    AppState::StreamingQuery { manager, sql, timeout_secs } => (manager, sql, timeout_secs),
                    other => {
                        self.state = other;
                        continue;
                    }
                };

                // ストリーミング表示（SQLエラー・タイムアウト時は?でErrを返してrun_loopに伝播）
                // LeaveAlternateScreen は show_result_streaming 内でサンプリング完了後に行う（ちらつき防止）
                let streaming_result = self.show_result_streaming(
                    manager.pool().clone(),
                    &sql,
                    std::time::Duration::from_secs(timeout_secs),
                    terminal,
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
                        self.query_input = next_sql;
                        self.cursor_position = self.query_input.chars().count();
                        self.add_to_history(&self.query_input.clone());
                        let sql_upper = self.query_input.trim().to_uppercase();
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
                        self.query_input = where_template;
                        self.cursor_position = self.query_input.chars().count();
                    }
                    None => {
                        self.state = AppState::Connected { manager };
                        self.query_input.clear();
                        self.cursor_position = 0;
                    }
                }

                // 状態遷移直後に即描画してちらつきを抑制する
                terminal
                    .draw(|f| self.render(f))
                    .map_err(|e| Error::Tui(format!("描画エラー: {}", e)))?;

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

                // カラム選択（DBエラー時は Error 状態に遷移）
                // current_database を渡して USE 後のDBのテーブル一覧を正しく表示する
                // LeaveAlternateScreen は select_columns_interactive 内でデータ準備後に行う（ちらつき防止）
                let select_result = self.select_columns_interactive(
                    manager.pool(),
                    std::time::Duration::from_secs(timeout_secs),
                    self.current_database.as_deref(),
                    terminal,
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
                        self.query_input = sql;
                        self.cursor_position = self.query_input.chars().count();
                        self.execute_query()?;
                    }
                    Ok(None) => {
                        // キャンセル: Connected 状態に戻るだけ
                        self.state = AppState::Connected { manager };
                    }
                }

                // 状態遷移直後に即描画してちらつきを抑制する
                terminal
                    .draw(|f| self.render(f))
                    .map_err(|e| Error::Tui(format!("描画エラー: {}", e)))?;

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

                // skimで結果表示（LeaveAlternateScreen は show_result_with_skim 内でデータ準備後に行う）
                let next_query = self.show_result_with_skim(&result, terminal)?;

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
                            self.query_input = sql;
                            self.cursor_position = self.query_input.chars().count();
                            self.add_to_history(&self.query_input.clone());
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
                            self.query_input = where_template;
                            self.cursor_position = self.query_input.chars().count();
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

                // 状態遷移直後に即描画してちらつきを抑制する
                terminal
                    .draw(|f| self.render(f))
                    .map_err(|e| Error::Tui(format!("描画エラー: {}", e)))?;

                continue;
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
                // abort() のみ呼び take() しない（run() 側で is_some() を確認して process::exit するため）
                if let Some(ref task) = self.connecting_task {
                    task.abort();
                }
                break;
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

        let query = self.query_input.clone();
        let pool = manager.pool().clone();
        let query_for_task = query.clone();
        // プールのセッション状態問題を回避するため、現在のデータベースをキャプチャしておく。
        // クエリ実行は別タスクで行われるため、クロージャにムーブする必要がある。
        let current_database_for_task = self.current_database.clone();

        // 次の show_result_with_skim でテーブル名を抽出できるよう保存する
        self.last_sql = query.clone();

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
                let error_message = t!(TuiMsg::QueryFailed { detail: &e.user_message() });
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
                    self.query_input.clear();
                    self.cursor_position = 0;
                } else {
                    // USE/SET等の結果を表示しないコマンドは即座にConnected状態に戻る
                    // USEコマンドの場合は選択データベースを更新する
                    self.update_current_database();
                    tracing::debug!("Command executed, returning to Connected state");
                    self.state = AppState::Connected { manager };
                    self.query_input.clear();
                    self.cursor_position = 0;

                    // USE実行後はテーブルキャッシュを更新する（新しいDBのテーブル一覧を取得）
                    // self.current_database は update_current_database() で更新済みのため、
                    // クローンして spawn に渡すことで正しいDBのテーブル一覧を取得できる
                    if let AppState::Connected { ref manager } = self.state {
                        let cache_arc = self.completion_cache.clone();
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
                    t!(TuiMsg::QueryTaskFailed { detail: &join_error.to_string() })
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

    /// 接続バックグラウンドタスクの完了をポーリングし、完了したら Connected または Error 状態に遷移する
    async fn poll_connecting(&mut self) -> Result<()> {
        if !matches!(self.state, AppState::Connecting { .. }) {
            return Ok(());
        }

        let finished = self
            .connecting_task
            .as_ref()
            .is_some_and(|t| t.is_finished());

        if !finished {
            // 接続中はスピナーフレームを進める
            if let AppState::Connecting { ref mut spinner_frame, .. } = self.state {
                *spinner_frame = spinner_frame.wrapping_add(1);
            }
            return Ok(());
        }

        let Some(task) = self.connecting_task.take() else {
            return Ok(());
        };

        let connection_name = match &self.state {
            AppState::Connecting { connection_name, .. } => connection_name.clone(),
            _ => return Ok(()),
        };

        let connections = self.connections.clone();

        match task.await {
            Ok(Ok(manager)) => {
                tracing::info!("Connection established: {}", connection_name);
                let cache_arc = self.completion_cache.clone();
                let pool = manager.pool().clone();
                tokio::spawn(async move {
                    if let Err(e) = initialize_completion_cache(&cache_arc, &pool, None).await {
                        tracing::warn!("補完キャッシュの初期化に失敗しました: {}", e);
                    }
                });
                self.state = AppState::Connected { manager };
            }
            Ok(Err(e)) => {
                tracing::error!("Connection failed: {}", e);
                self.state = AppState::Error {
                    message: e.user_message(),
                    previous_state: Box::new(AppState::Selecting {
                        connections,
                        selected_index: 0,
                    }),
                };
            }
            Err(join_error) => {
                tracing::error!("Connection task panicked: {}", join_error);
                self.state = AppState::Error {
                    message: format!("接続タスクが異常終了しました: {}", join_error),
                    previous_state: Box::new(AppState::Selecting {
                        connections,
                        selected_index: 0,
                    }),
                };
            }
        }

        Ok(())
    }

    /// 実行中クエリがあれば中断する
    fn abort_running_query(&mut self) {
        if let Some(running_query) = self.running_query.take() {
            tracing::info!("Aborting running query task");
            running_query.task.abort();
        }
    }
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
        sqlx::query(&format!("SHOW TABLES FROM {}", crate::query::escape_identifier(db)))
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

    tracing::debug!(
        "Table cache refreshed: {} tables",
        cache_write.tables.len()
    );

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

    /// テスト用に query_input のみをセットした最小限の App を生成する
    ///
    /// App::new() は Config 等の複雑な依存があるため、テストでは
    /// 必要なフィールドのみをセットした App を直接構築する。
    fn make_app_with_input(input: &str) -> App {
        App {
            state: AppState::Selecting {
                connections: Vec::new(),
                selected_index: 0,
            },
            query_input: input.to_string(),
            should_quit: false,
            running_query: None,
            selected_record: None,
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            cursor_position: 0,
            selection_start: None,
            last_sql: String::new(),
            sql_history: std::collections::VecDeque::new(),
            history_index: None,
            history_draft: String::new(),
            current_database: None,
            connection_name: None,
            current_table: None,
            readonly: false,
            kill_buffer: String::new(),
            completion_cache: Arc::new(tokio::sync::RwLock::new(
                crate::completion::CompletionCache::new(),
            )),
            completion_state: None,
            connections: Vec::new(),
            connecting_task: None,
        }
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
}
