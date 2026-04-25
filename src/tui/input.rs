use crossterm::event::{self, Event, KeyCode, KeyModifiers};

use crate::error::Result;
use crate::i18n::TuiMsg;
use crate::t;

use super::{App, AppState, CompletionState, MAX_SQL_HISTORY, is_write_sql};

impl App {
    /// イベント処理
    pub(super) async fn handle_event(&mut self, event: Event) -> Result<()> {
        if let Event::Key(key_event) = event {
            // Ctrl+C で終了（全状態共通）
            // Connected状態では選択範囲コピー処理を優先するため handle_connected_input に委譲する
            if key_event.code == KeyCode::Char('c')
                && key_event.modifiers.contains(KeyModifiers::CONTROL)
                && !matches!(self.state, AppState::Connected { .. }) {
                // abort() のみ呼び take() しない（run() 側で is_some() を確認して process::exit するため）
                if let Some(ref task) = self.connecting_task {
                    task.abort();
                }
                self.should_quit = true;
                return Ok(());
                // Connected状態の場合はhandle_connected_inputに流す
            }

            // 状態別のキーハンドリング
            match &self.state {
                AppState::Selecting { .. } => {
                    // Selecting状態はrun()で既に処理済みのため到達しない
                }
                AppState::Connected { .. } => self.handle_connected_input(key_event).await?,
                AppState::Connecting { .. }
                | AppState::Executing { .. }
                | AppState::StreamingQuery { .. }
                | AppState::SelectingColumns { .. } => {
                    // 接続中・実行中・ストリーミング待ち・カラム選択中は入力を受け付けない
                }
                // ShowingResultはskimへの即遷移トリガーのみで使われるため、キー入力は到達しない
                AppState::ShowingResult { .. } => {}
                AppState::Error { .. } => self.handle_error_input(key_event),
            }
        }

        Ok(())
    }

    /// 接続済み状態のキー入力処理
    pub(super) async fn handle_connected_input(&mut self, key_event: event::KeyEvent) -> Result<()> {
        // 補完ポップアップ表示中は専用ハンドラに優先委譲する
        // 補完ハンドラが消費したキーは以降の処理に伝播させない
        if self.completion_state.is_some()
            && self.handle_completion_key(key_event).await?
        {
            return Ok(());
        }

        match key_event.code {
            // Ctrl+D: SHOW DATABASES をストリーミング表示
            KeyCode::Char('d') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query_input = "SHOW DATABASES".to_string();
                self.cursor_position = self.query_input.chars().count();
                self.selected_record = None;
                self.selection_start = None;
                self.transition_to_streaming()?;
            }
            // Ctrl+T: SHOW TABLES をストリーミング表示
            KeyCode::Char('t') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query_input = "SHOW TABLES".to_string();
                self.cursor_position = self.query_input.chars().count();
                self.selected_record = None;
                self.selection_start = None;
                self.transition_to_streaming()?;
            }
            // Ctrl+S: skimベースのカラム選択（SHOW TABLES → テーブル選択 → SHOW COLUMNS → カラム選択）
            KeyCode::Char('s') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                self.selected_record = None;
                self.selection_start = None;
                self.transition_to_column_select()?;
            }
            // Ctrl+C: 選択範囲をクリップボードにコピー（選択なしの場合は終了）
            KeyCode::Char('c') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some((byte_start, byte_end)) = self.selection_byte_range() {
                    let selected_text = &self.query_input[byte_start..byte_end];
                    if let Ok(mut clipboard) = arboard::Clipboard::new() {
                        let _ = clipboard.set_text(selected_text.to_string());
                    }
                    // コピー後は選択解除
                    self.selection_start = None;
                } else {
                    // 選択範囲がない場合は従来のCtrl+C動作（終了）
                    self.should_quit = true;
                }
            }
            // Ctrl+X: 選択範囲をカット
            KeyCode::Char('x') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some((byte_start, byte_end)) = self.selection_byte_range() {
                    let selected_text = self.query_input[byte_start..byte_end].to_string();
                    if let Ok(mut clipboard) = arboard::Clipboard::new() {
                        let _ = clipboard.set_text(selected_text);
                    }
                    // 選択範囲を削除してカーソルを選択開始位置に移動
                    let cursor_start = self.selection_start.unwrap_or(self.cursor_position).min(self.cursor_position);
                    self.query_input.replace_range(byte_start..byte_end, "");
                    self.cursor_position = cursor_start;
                    self.selection_start = None;
                }
            }
            // Ctrl+V: クリップボードからペースト
            // SQL入力欄は1行のため、改行文字はスペースに変換して挿入する
            KeyCode::Char('v') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                // 選択範囲があれば先に削除（上書きペースト）
                self.delete_selection();
                if let Ok(mut clipboard) = arboard::Clipboard::new() {
                    if let Ok(text) = clipboard.get_text() {
                        let sanitized = text.replace('\n', " ").replace('\r', "");
                        let byte_pos = self.char_to_byte(self.cursor_position);
                        self.query_input.insert_str(byte_pos, &sanitized);
                        self.cursor_position += sanitized.chars().count();
                    }
                }
            }
            // Ctrl+A: 全選択
            // 従来の「カーソルを行頭へ」から変更し、全テキストを選択状態にする
            KeyCode::Char('a') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                self.selection_start = Some(0);
                self.cursor_position = self.query_input.chars().count();
            }
            // Ctrl+E: カーソルを行末へ
            KeyCode::Char('e') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor_position = self.query_input.chars().count();
                self.selection_start = None;
            }
            // Ctrl+K: カーソル位置から行末までを削除（kill-line）
            // 選択範囲がある場合は選択範囲を削除する
            KeyCode::Char('k') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.selection_start.is_some() {
                    self.delete_selection();
                } else {
                    let byte_start = self.char_to_byte(self.cursor_position);
                    // 削除範囲を kill_buffer に保存する
                    self.kill_buffer = self.query_input[byte_start..].to_string();
                    self.query_input.truncate(byte_start);
                    // cursor_position はそのまま（行末に到達した状態）
                }
            }
            // Ctrl+W: カーソル直前の単語を削除（bash readline backward-kill-word）
            // Opt+Backspace と同じ動作（Linux環境での互換性）
            KeyCode::Char('w') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.selection_start.is_some() {
                    self.delete_selection();
                } else {
                    let new_pos = self.word_left(self.cursor_position);
                    if new_pos < self.cursor_position {
                        let byte_start = self.char_to_byte(new_pos);
                        let byte_end = self.char_to_byte(self.cursor_position);
                        self.query_input.replace_range(byte_start..byte_end, "");
                        self.cursor_position = new_pos;
                    }
                }
            }
            // Ctrl+U: 行頭からカーソル位置までを削除（kill-whole-line 前半）
            // 選択範囲がある場合は選択範囲を削除する
            KeyCode::Char('u') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.selection_start.is_some() {
                    self.delete_selection();
                } else {
                    let byte_end = self.char_to_byte(self.cursor_position);
                    // 削除範囲を kill_buffer に保存する
                    self.kill_buffer = self.query_input[..byte_end].to_string();
                    self.query_input.replace_range(..byte_end, "");
                    self.cursor_position = 0;
                }
            }
            // Ctrl+Y: キルバッファからペースト（yank）
            // Ctrl+V はシステムクリップボードからのペーストのまま維持する
            KeyCode::Char('y') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                self.delete_selection();
                if !self.kill_buffer.is_empty() {
                    let byte_pos = self.char_to_byte(self.cursor_position);
                    let yanked = self.kill_buffer.clone();
                    self.query_input.insert_str(byte_pos, &yanked);
                    self.cursor_position += yanked.chars().count();
                }
            }
            // Opt+Shift+← / Alt+Shift+←: 選択しながら1単語左へ移動
            // ALT+SHIFT 複合修飾子は ALT 単独・SHIFT 単独より前に配置してマッチ優先度を確保する
            KeyCode::Left
                if key_event.modifiers.contains(KeyModifiers::ALT)
                    && key_event.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                if self.selection_start.is_none() {
                    self.selection_start = Some(self.cursor_position);
                }
                self.cursor_position = self.word_left(self.cursor_position);
            }
            // Opt+Shift+→ / Alt+Shift+→: 選択しながら1単語右へ移動
            KeyCode::Right
                if key_event.modifiers.contains(KeyModifiers::ALT)
                    && key_event.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                if self.selection_start.is_none() {
                    self.selection_start = Some(self.cursor_position);
                }
                self.cursor_position = self.word_right(self.cursor_position);
            }
            // Shift+Left: 選択しながら左へ移動
            // 通常のLeftより前に配置してShift修飾子付きが先にマッチするようにする
            KeyCode::Left if key_event.modifiers.contains(KeyModifiers::SHIFT) => {
                if self.cursor_position > 0 {
                    // 選択中でなければ現在位置を選択開始点として記録
                    if self.selection_start.is_none() {
                        self.selection_start = Some(self.cursor_position);
                    }
                    self.cursor_position -= 1;
                }
            }
            // Shift+Right: 選択しながら右へ移動
            KeyCode::Right if key_event.modifiers.contains(KeyModifiers::SHIFT) => {
                let char_count = self.query_input.chars().count();
                if self.cursor_position < char_count {
                    if self.selection_start.is_none() {
                        self.selection_start = Some(self.cursor_position);
                    }
                    self.cursor_position += 1;
                }
            }
            // Shift+Home: 選択しながら行頭へ
            KeyCode::Home if key_event.modifiers.contains(KeyModifiers::SHIFT) => {
                if self.selection_start.is_none() {
                    self.selection_start = Some(self.cursor_position);
                }
                self.cursor_position = 0;
            }
            // Shift+End: 選択しながら行末へ
            KeyCode::End if key_event.modifiers.contains(KeyModifiers::SHIFT) => {
                if self.selection_start.is_none() {
                    self.selection_start = Some(self.cursor_position);
                }
                self.cursor_position = self.query_input.chars().count();
            }
            // Opt+← / Alt+←: 1単語左へ移動（選択解除）
            // iTerm2 / crossterm 標準パターン
            KeyCode::Left if key_event.modifiers.contains(KeyModifiers::ALT) => {
                self.selection_start = None;
                self.cursor_position = self.word_left(self.cursor_position);
            }
            // ESC b（Terminal.app の meta-key 送信形式）
            KeyCode::Char('b') if key_event.modifiers.contains(KeyModifiers::ALT) => {
                self.selection_start = None;
                self.cursor_position = self.word_left(self.cursor_position);
            }
            // Opt+→ / Alt+→: 1単語右へ移動（選択解除）
            // iTerm2 / crossterm 標準パターン
            KeyCode::Right if key_event.modifiers.contains(KeyModifiers::ALT) => {
                self.selection_start = None;
                self.cursor_position = self.word_right(self.cursor_position);
            }
            // ESC f（Terminal.app の meta-key 送信形式）
            KeyCode::Char('f') if key_event.modifiers.contains(KeyModifiers::ALT) => {
                self.selection_start = None;
                self.cursor_position = self.word_right(self.cursor_position);
            }
            // query_inputが空の時のみqで終了（SQL入力中はqを文字として扱う）
            KeyCode::Char('q') if self.query_input.is_empty() => {
                self.should_quit = true;
            }
            KeyCode::Char(c) => {
                // 選択範囲があれば先に削除（選択テキストを上書き入力）
                self.delete_selection();
                // カーソル位置（char単位）をバイト位置に変換して文字を挿入する
                let byte_pos = self.char_to_byte(self.cursor_position);
                self.query_input.insert(byte_pos, c);
                self.cursor_position += 1;
            }
            // Opt+Backspace / Alt+Backspace: カーソル直前の単語を削除
            // 選択範囲がある場合は選択範囲を削除する
            KeyCode::Backspace if key_event.modifiers.contains(KeyModifiers::ALT) => {
                if self.selection_start.is_some() {
                    self.delete_selection();
                } else {
                    let new_pos = self.word_left(self.cursor_position);
                    if new_pos < self.cursor_position {
                        let byte_start = self.char_to_byte(new_pos);
                        let byte_end = self.char_to_byte(self.cursor_position);
                        self.query_input.replace_range(byte_start..byte_end, "");
                        self.cursor_position = new_pos;
                    }
                }
            }
            KeyCode::Backspace => {
                if self.selection_start.is_some() {
                    // 選択範囲があれば選択範囲全体を削除
                    self.delete_selection();
                } else {
                    // カーソル位置の直前の文字を削除する
                    if self.cursor_position > 0 {
                        let byte_pos = self.char_to_byte(self.cursor_position - 1);
                        self.query_input.remove(byte_pos);
                        self.cursor_position -= 1;
                    }
                }
            }
            // Opt+Delete / Alt+Delete: カーソル直後の単語を削除
            // 選択範囲がある場合は選択範囲を削除する
            KeyCode::Delete if key_event.modifiers.contains(KeyModifiers::ALT) => {
                if self.selection_start.is_some() {
                    self.delete_selection();
                } else {
                    let new_pos = self.word_right(self.cursor_position);
                    if new_pos > self.cursor_position {
                        let byte_start = self.char_to_byte(self.cursor_position);
                        let byte_end = self.char_to_byte(new_pos);
                        self.query_input.replace_range(byte_start..byte_end, "");
                        // cursor_position はそのまま（次の単語が繰り上がる）
                    }
                }
            }
            KeyCode::Delete => {
                if self.selection_start.is_some() {
                    // 選択範囲があれば選択範囲全体を削除
                    self.delete_selection();
                } else {
                    // カーソル位置の文字を削除する（Deleteキー）
                    let char_count = self.query_input.chars().count();
                    if self.cursor_position < char_count {
                        let byte_pos = self.char_to_byte(self.cursor_position);
                        self.query_input.remove(byte_pos);
                        // cursor_positionはそのまま（次の文字が繰り上がる）
                    }
                }
            }
            KeyCode::Left => {
                self.selection_start = None;
                if self.cursor_position > 0 {
                    self.cursor_position -= 1;
                }
            }
            KeyCode::Right => {
                self.selection_start = None;
                let char_count = self.query_input.chars().count();
                if self.cursor_position < char_count {
                    self.cursor_position += 1;
                }
            }
            KeyCode::Home => {
                self.selection_start = None;
                self.cursor_position = 0;
            }
            KeyCode::End => {
                self.selection_start = None;
                self.cursor_position = self.query_input.chars().count();
            }
            KeyCode::Enter => {
                if !self.query_input.trim().is_empty() {
                    // sc エイリアス: カラム選択モードに遷移（expand_aliases より前に処理する）
                    if self.query_input.trim().to_lowercase() == "sc" {
                        self.query_input.clear();
                        self.cursor_position = 0;
                        self.selected_record = None;
                        self.selection_start = None;
                        self.transition_to_column_select()?;
                        return Ok(());
                    }

                    // 新しいクエリ実行時は選択レコードプレビューをクリア
                    self.selected_record = None;
                    // エイリアス展開してから実行
                    self.expand_aliases();

                    self.add_to_history(&self.query_input.clone());

                    // エイリアス展開後にカーソルを末尾に合わせる
                    self.cursor_position = self.query_input.chars().count();

                    // readonlyモード: 書き込み系SQLをクライアント側で即座にブロックする
                    // サーバー側でもブロックされるが、ユーザーへの即時フィードバックのため先にチェックする
                    if self.is_current_readonly() && is_write_sql(&self.query_input) {
                        let current_state = std::mem::replace(
                            &mut self.state,
                            AppState::Selecting {
                                connections: Vec::new(),
                                selected_index: 0,
                            },
                        );
                        self.state = AppState::Error {
                            message: t!(TuiMsg::ReadonlyBlocked),
                            previous_state: Box::new(current_state),
                        };
                        return Ok(());
                    }

                    let sql_upper = self.query_input.trim().to_uppercase();
                    if sql_upper.starts_with("USE ") || sql_upper.starts_with("SET ") {
                        // 非表示コマンドは従来通りバックグラウンドクエリ実行
                        self.execute_query()?;
                    } else {
                        // 表示クエリはストリーミングモードへ遷移
                        self.transition_to_streaming()?;
                    }
                }
            }
            // ↑キー: 履歴を遡る（古い方向へ）
            KeyCode::Up => {
                if self.sql_history.is_empty() {
                    return Ok(());
                }
                match self.history_index {
                    None => {
                        // 新規入力中 → 現在の入力を退避して最新の履歴を表示
                        self.history_draft = self.query_input.clone();
                        let idx = self.sql_history.len() - 1;
                        self.history_index = Some(idx);
                        self.query_input = self.sql_history[idx].clone();
                    }
                    Some(idx) if idx > 0 => {
                        // 履歴参照中 → さらに古い履歴へ
                        let new_idx = idx - 1;
                        self.history_index = Some(new_idx);
                        self.query_input = self.sql_history[new_idx].clone();
                    }
                    _ => {
                        // 最古の履歴に到達済み → 何もしない
                        return Ok(());
                    }
                }
                self.cursor_position = self.query_input.chars().count();
                self.selection_start = None;
            }
            // ↓キー: 履歴を進む（新しい方向へ）
            KeyCode::Down => {
                match self.history_index {
                    Some(idx) => {
                        if idx + 1 < self.sql_history.len() {
                            // より新しい履歴へ
                            let new_idx = idx + 1;
                            self.history_index = Some(new_idx);
                            self.query_input = self.sql_history[new_idx].clone();
                        } else {
                            // 履歴の末尾を超えた → 退避した入力を復元して新規入力状態に戻す
                            self.history_index = None;
                            self.query_input = self.history_draft.clone();
                            self.history_draft.clear();
                        }
                        self.cursor_position = self.query_input.chars().count();
                        self.selection_start = None;
                    }
                    None => {
                        // 新規入力中 → 何もしない
                    }
                }
            }
            // WHEREテンプレート+レコードプレビュー表示中にESCで通常のSQL入力画面に戻る
            KeyCode::Esc => {
                self.query_input.clear();
                self.cursor_position = 0;
                self.selected_record = None;
            }
            _ => {}
        }

        // 文字入力・Backspace・Delete の後に補完候補を更新する
        // 非入力キー（移動キー等）では候補が変わらないだけで副作用はない
        self.update_completion_state().await;

        Ok(())
    }

    /// 補完ポップアップ表示中のキー処理
    ///
    /// キーを消費した場合 true を返す（呼び出し元がこれ以上処理しないよう通知）
    pub(super) async fn handle_completion_key(&mut self, key_event: event::KeyEvent) -> Result<bool> {
        match key_event.code {
            // Tab / ↓: 次の候補へ（ラップアラウンド）
            KeyCode::Tab | KeyCode::Down => {
                if let Some(ref mut state) = self.completion_state {
                    if state.candidates.is_empty() {
                        self.completion_state = None;
                    } else {
                        state.selected_index =
                            (state.selected_index + 1) % state.candidates.len();
                    }
                }
                return Ok(true);
            }
            // Shift+Tab: 前の候補へ（ラップアラウンド）
            KeyCode::BackTab => {
                if let Some(ref mut state) = self.completion_state {
                    if !state.candidates.is_empty() {
                        let len = state.candidates.len();
                        state.selected_index = (state.selected_index + len - 1) % len;
                    }
                }
                return Ok(true);
            }
            // ↑（ポップアップ表示中のみ）: 前の候補へ
            // 履歴ナビゲーションより補完を優先する
            KeyCode::Up if self.completion_state.is_some() => {
                if let Some(ref mut state) = self.completion_state {
                    if !state.candidates.is_empty() {
                        let len = state.candidates.len();
                        state.selected_index = (state.selected_index + len - 1) % len;
                    }
                }
                return Ok(true);
            }
            // Enter（ポップアップ表示中のみ）: 候補確定（SQL実行は行わない）
            KeyCode::Enter if self.completion_state.is_some() => {
                self.confirm_completion();
                return Ok(true);
            }
            // Esc: ポップアップを閉じる（非表示時は既存の Esc 処理へ流す）
            KeyCode::Esc => {
                if self.completion_state.is_some() {
                    self.completion_state = None;
                    return Ok(true);
                }
                return Ok(false);
            }
            _ => {}
        }
        Ok(false)
    }

    /// 選択中の補完候補を確定して入力欄に挿入する
    pub(super) fn confirm_completion(&mut self) {
        let Some(state) = self.completion_state.take() else {
            return;
        };
        let Some(item) = state.candidates.get(state.selected_index) else {
            return;
        };

        // カーソル位置のバイトオフセットを計算して現在トークンを置換する
        let cursor_byte = self.char_to_byte(self.cursor_position);
        let (_, token_start_byte) =
            crate::completion::current_token_with_pos(&self.query_input, cursor_byte);

        // 既存トークンを削除して補完テキストを挿入する
        let completion_text = item.text.clone();
        self.query_input
            .replace_range(token_start_byte..cursor_byte, &completion_text);
        // 挿入後のカーソル位置 = トークン開始位置 + 挿入テキストのchar数
        let inserted_chars = completion_text.chars().count();
        let token_start_char = self.query_input[..token_start_byte].chars().count();
        self.cursor_position = token_start_char + inserted_chars;
        // 選択状態をリセット（補完後に選択範囲が残らないようにする）
        self.selection_start = None;
    }

    /// 補完候補リストを現在の入力状態に基づいて更新する
    ///
    /// 文字入力・削除のたびに呼ばれる。キャッシュはRwLock経由で非同期に参照する。
    pub(super) async fn update_completion_state(&mut self) {
        let cursor_byte = self.char_to_byte(self.cursor_position);
        let sql_before = self.query_input[..cursor_byte].to_string();
        let (token_ref, token_start) =
            crate::completion::current_token_with_pos(&self.query_input, cursor_byte);
        let token = token_ref.to_string();

        // トークンが空でポップアップ非表示の場合は表示しない（1文字目から補完を表示する）
        if token.is_empty() && self.completion_state.is_none() {
            return;
        }
        // トークンが空でポップアップ表示中の場合は閉じる
        if token.is_empty() {
            self.completion_state = None;
            return;
        }

        let context = crate::completion::analyze_context(&sql_before);

        // db.table パターンの検出: トークン開始位置の直前が '.' の場合
        // 例: "SELECT * FROM warehouse.b" → qualified_db = Some("warehouse")
        let qualified_db: Option<String> = if token_start > 0 {
            let before_dot = &self.query_input[..token_start];
            if let Some(db_part) = before_dot.strip_suffix('.') {
                db_part
                    .rsplit(|c: char| c.is_whitespace())
                    .next()
                    .map(|s| s.trim_matches('`').to_string())
                    .filter(|s| !s.is_empty())
            } else {
                None
            }
        } else {
            None
        };

        // db.table パターン（qualified_db が Some）の場合は DatabaseTableName コンテキストに変換する。
        // DB未選択かつ db.prefix なしの場合のみ DatabaseName に変換する。
        let context = if let Some(ref db_name) = qualified_db {
            // db.table パターン: 指定DBのテーブル名コンテキストに変換する
            crate::completion::SqlContext::DatabaseTableName { database: db_name.clone() }
        } else if self.current_database.is_none()
            && context == crate::completion::SqlContext::TableName
        {
            // データベース未選択（USEしていない）時にテーブル名コンテキストの場合、
            // テーブル名の代わりにデータベース名の候補を表示する（db.table形式の入力を想定）
            crate::completion::SqlContext::DatabaseName
        } else {
            context
        };

        // db.table パターン: 指定DBのテーブルキャッシュを取得する
        // spawnではなくawaitで完了を待つことで、キャッシュ未充填のまま候補生成に進むのを防ぐ
        // キャッシュ済みの場合は即座にreturnするため2回目以降のパフォーマンス影響はない
        if let crate::completion::SqlContext::DatabaseTableName { ref database } = context {
            let cache_arc = self.completion_cache.clone();
            let db_name_clone = database.clone();
            if let AppState::Connected { ref manager } = self.state {
                let pool = manager.pool().clone();
                crate::completion::fetch_database_tables_if_needed(
                    &cache_arc,
                    &pool,
                    &db_name_clone,
                )
                .await;
            }
        }

        // カラム補完でテーブルが特定された場合、カラムキャッシュを取得する
        // spawnではなくawaitで完了を待つことで、キャッシュ未充填のまま候補生成に進むのを防ぐ
        // キャッシュ済みの場合は即座にreturnするため2回目以降のパフォーマンス影響はない
        if let crate::completion::SqlContext::ColumnName {
            table: Some(ref table_name),
        } = context
        {
            let cache_arc = self.completion_cache.clone();
            let table_name = table_name.clone();
            if let AppState::Connected { ref manager } = self.state {
                let pool = manager.pool().clone();
                crate::completion::fetch_column_cache_if_needed(
                    &cache_arc,
                    &pool,
                    &table_name,
                )
                .await;
            }
        }

        let cache = self.completion_cache.read().await;

        let candidates = crate::completion::get_candidates(&token, &context, &cache);

        if candidates.is_empty() {
            self.completion_state = None;
            return;
        }

        // 既存ポップアップの選択位置を維持しつつ候補リストを更新する
        let selected_index = self
            .completion_state
            .as_ref()
            .map(|s| s.selected_index.min(candidates.len().saturating_sub(1)))
            .unwrap_or(0);

        self.completion_state = Some(CompletionState {
            candidates,
            selected_index,
            current_token: token,
        });
    }

    /// SQL実行履歴に追加する
    ///
    /// 直前と同じクエリは重複追加しない。最大MAX_SQL_HISTORY件を保持する。
    pub(super) fn add_to_history(&mut self, sql: &str) {
        let sql = sql.trim().to_string();
        if sql.is_empty() {
            return;
        }
        if self.sql_history.back().map(|s| s.as_str()) != Some(&sql) {
            self.sql_history.push_back(sql);
            if self.sql_history.len() > MAX_SQL_HISTORY {
                self.sql_history.pop_front();
            }
        }
        // 履歴参照状態をリセット（実行後は新規入力状態に戻す）
        self.history_index = None;
        self.history_draft.clear();
    }

    /// カーソルを1単語左に移動した時の位置（char単位）を返す
    ///
    /// bash readline の backward-word に相当する動作:
    /// 1. 現在位置の直前にある区切り文字をスキップ
    /// 2. 単語文字が続く間遡る
    pub(super) fn word_left(&self, from: usize) -> usize {
        if from == 0 {
            return 0;
        }
        let chars: Vec<char> = self.query_input.chars().collect();
        let mut pos = from;

        // まず区切り文字をスキップ
        while pos > 0 && crate::completion::is_completion_separator(chars[pos - 1]) {
            pos -= 1;
        }
        // 次に単語文字を遡る
        while pos > 0 && !crate::completion::is_completion_separator(chars[pos - 1]) {
            pos -= 1;
        }
        pos
    }

    /// カーソルを1単語右に移動した時の位置（char単位）を返す
    ///
    /// bash readline の forward-word に相当する動作:
    /// 1. 現在位置の直後にある区切り文字をスキップ
    /// 2. 単語文字が続く間進む
    pub(super) fn word_right(&self, from: usize) -> usize {
        let chars: Vec<char> = self.query_input.chars().collect();
        let len = chars.len();
        if from >= len {
            return len;
        }
        let mut pos = from;

        // まず区切り文字をスキップ
        while pos < len && crate::completion::is_completion_separator(chars[pos]) {
            pos += 1;
        }
        // 次に単語文字を進む
        while pos < len && !crate::completion::is_completion_separator(chars[pos]) {
            pos += 1;
        }
        pos
    }

    /// query_inputのchar位置をバイト位置に変換する
    ///
    /// char_indices()でO(n)だが入力文字列は通常短いため許容範囲。
    pub(super) fn char_to_byte(&self, char_pos: usize) -> usize {
        self.query_input
            .char_indices()
            .nth(char_pos)
            .map(|(i, _)| i)
            .unwrap_or(self.query_input.len())
    }

    /// 選択範囲のバイト範囲を返す（選択なしの場合はNone）
    pub(super) fn selection_byte_range(&self) -> Option<(usize, usize)> {
        let sel_start = self.selection_start?;
        let start = sel_start.min(self.cursor_position);
        let end = sel_start.max(self.cursor_position);
        // start==0のときchar_to_byteは0を返すが、unwrap_or(0)と等価
        let byte_start = self.char_to_byte(start);
        let byte_end = self.char_to_byte(end);
        Some((byte_start, byte_end))
    }

    /// 選択範囲のテキストを削除し、カーソルを選択開始位置に移動する
    ///
    /// selection_startがNoneの場合は何もしない。
    /// 呼び出し後はselection_startがNoneになる。
    pub(super) fn delete_selection(&mut self) {
        if let Some((byte_start, byte_end)) = self.selection_byte_range() {
            let start = self.selection_start.unwrap_or(self.cursor_position).min(self.cursor_position);
            self.query_input.replace_range(byte_start..byte_end, "");
            self.cursor_position = start;
            self.selection_start = None;
        }
    }

    /// SQL入力のエイリアスを展開する
    ///
    /// 短縮形を完全なSQL文に変換することで入力効率を向上させる
    pub(super) fn expand_aliases(&mut self) {
        let expanded = match self.query_input.trim().to_lowercase().as_str() {
            "sd" => "SHOW DATABASES".to_string(),
            "st" => "SHOW TABLES".to_string(),
            _ => return,
        };
        self.query_input = expanded;
    }

    /// エラー状態のキー入力処理
    pub(super) fn handle_error_input(&mut self, key_event: event::KeyEvent) {
        match key_event.code {
            KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => {
                let is_connecting_error = matches!(
                    &self.state,
                    AppState::Error { previous_state, .. }
                    if matches!(previous_state.as_ref(), AppState::Selecting { .. })
                );

                if is_connecting_error {
                    // 接続失敗エラーはアプリ終了（シェルに戻る）
                    self.should_quit = true;
                } else {
                    // SQL実行エラー等はConnected状態に戻る
                    let previous_state = match std::mem::replace(
                        &mut self.state,
                        AppState::Selecting {
                            connections: Vec::new(),
                            selected_index: 0,
                        },
                    ) {
                        AppState::Error { previous_state, .. } => *previous_state,
                        other => other,
                    };
                    self.state = previous_state;
                }
            }
            _ => {}
        }
    }
}
