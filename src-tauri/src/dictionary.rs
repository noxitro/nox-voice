//! ユーザー辞書 — 固有名詞・専門用語の表記を安定させる。
//!
//! 1 行 1 語。`表記` だけの行と、`表記,よみ` の行を受け付ける
//! (区切りは半角/全角どちらのカンマでもよい)。
//!
//! ```text
//! nox-voice
//! 塩谷,しおや
//! ```
//!
//! 辞書は 2 か所で使う:
//!
//! - **STT (Whisper) の `prompt`**: 音になる前の段階で表記を寄せる。
//!   誤変換をそもそも起こさせないので、整形での修復より確実。
//! - **整形 (Gemini) のプロンプト**: 転写後の表記ゆれを直す。
//!   `よみ` があれば「よみ → 表記」の変換指示として渡せる。

use serde::Serialize;

/// Whisper の `prompt` に使える文字数の目安。
///
/// API 側の上限は **224 トークン**で、超えた分は**先頭から捨てられる**
/// (使われるのは末尾 224 トークン)。日本語は 1 文字が 1 トークン以上に
/// なりやすいので、文字数で保守的に見積もる。
/// かな漢字は 1 文字あたり 2 トークン前後になることがあるので、
/// 224 トークンに対して 150 文字と厳しめに見る。
/// この見積もりを外しても壊れないよう、**優先度の高いものほど末尾に置く**
/// ([`build_whisper_prompt`] 参照)。
pub const WHISPER_PROMPT_MAX_CHARS: usize = 150;

/// 辞書 1 語。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DictionaryEntry {
    /// 出力してほしい表記。
    pub written: String,
    /// 読み (任意)。音は合っているのに表記が違うケースを直すために使う。
    pub reading: Option<String>,
}

impl DictionaryEntry {
    /// 整形プロンプトへ 1 行で書ける形にする。
    pub fn as_instruction(&self) -> String {
        match &self.reading {
            Some(reading) => format!("{}(よみ: {}) ← この音は必ずこの表記にする", self.written, reading),
            None => self.written.clone(),
        }
    }
}

/// 設定の生の行を辞書エントリへ変換する。
///
/// 空行・空の表記は捨てる。区切り記号が複数あっても最初のものだけを見る
/// (表記自体にカンマを含めたい場合は読みを付けなければよい)。
pub fn parse_entries(lines: &[String]) -> Vec<DictionaryEntry> {
    lines
        .iter()
        .filter_map(|line| parse_entry(line))
        .collect()
}

fn parse_entry(line: &str) -> Option<DictionaryEntry> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    // 半角・全角どちらのカンマでも読みの区切りとみなす。
    let split = line.find(',').or_else(|| line.find('，'));
    let (written, reading) = match split {
        Some(idx) => {
            let (w, r) = line.split_at(idx);
            // 区切り文字ぶんを飛ばす (全角は 3 バイト)。
            let r = r.strip_prefix(',').or_else(|| r.strip_prefix('，')).unwrap_or("");
            (w.trim(), r.trim())
        }
        None => (line, ""),
    };

    if written.is_empty() {
        return None;
    }
    Some(DictionaryEntry {
        written: written.to_string(),
        reading: (!reading.is_empty()).then(|| reading.to_string()),
    })
}

/// Whisper の `prompt` を組み立てる。
///
/// # 並び順が仕様
///
/// 上限を超えた分は API 側で**先頭から**捨てられる。したがって
/// **落としたくないものほど後ろに置く**。ここでは
/// 「画面コンテキスト → 辞書」の順にして、溢れたときに削れるのが
/// 画面コンテキスト側になるようにしている (優先度: 辞書 > コンテキスト)。
///
/// 自前でも文字数を切り詰めるのは、サーバ側の切り捨てに頼ると
/// 「境界で語が割れて別の語になる」事故が起きるため。
pub fn build_whisper_prompt(entries: &[DictionaryEntry], context: Option<&str>) -> Option<String> {
    let terms: Vec<&str> = entries.iter().map(|e| e.written.as_str()).collect();
    // 語の途中で切れると別の語になるので、語単位で予算に収める。
    let dictionary = trim_terms_to_budget(&terms, WHISPER_PROMPT_MAX_CHARS);
    let dict_len = dictionary.chars().count();

    let context = context
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(collapse_whitespace);

    match (dictionary.is_empty(), context) {
        (true, None) => None,
        (true, Some(ctx)) => {
            let ctx = take_last_chars(&ctx, WHISPER_PROMPT_MAX_CHARS);
            (!ctx.is_empty()).then_some(ctx)
        }
        (false, None) => Some(dictionary),
        (false, Some(ctx)) => {
            // 辞書のぶんを引いた残りにコンテキストを入れる。
            let remaining = WHISPER_PROMPT_MAX_CHARS.saturating_sub(dict_len + 1);
            if remaining == 0 {
                return Some(dictionary);
            }
            let ctx = take_last_chars(&ctx, remaining);
            if ctx.is_empty() {
                Some(dictionary)
            } else {
                Some(format!("{ctx} {dictionary}"))
            }
        }
    }
}

/// 語単位で予算に収める。入る語が 1 つも無ければ空文字。
///
/// 予算を超える語に当たっても**打ち切らずに次を見る**。長い語が 1 つ
/// 先頭にあるだけで、後ろの短い語が全部落ちてしまうのを避けるため。
fn trim_terms_to_budget(terms: &[&str], budget: usize) -> String {
    let mut used = 0usize;
    let mut kept: Vec<&str> = Vec::new();
    for term in terms {
        let len = term.chars().count();
        let extra = if kept.is_empty() { len } else { len + 1 };
        if used + extra > budget {
            continue;
        }
        used += extra;
        kept.push(term);
    }
    kept.join("、")
}

/// 末尾から `max` 文字を取る (文字境界で切る)。
fn take_last_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    text.chars().skip(count - max).collect()
}

/// 連続する空白・改行を 1 個のスペースに畳む。
fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_was_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !last_was_space && !out.is_empty() {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.push(ch);
            last_was_space = false;
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // --- パース ---

    #[test]
    fn plain_terms_have_no_reading() {
        let entries = parse_entries(&lines(&["nox-voice", "Tauri"]));
        assert_eq!(
            entries,
            vec![
                DictionaryEntry { written: "nox-voice".into(), reading: None },
                DictionaryEntry { written: "Tauri".into(), reading: None },
            ]
        );
    }

    #[test]
    fn a_reading_can_follow_a_comma() {
        let entries = parse_entries(&lines(&["塩谷,しおや"]));
        assert_eq!(entries[0].written, "塩谷");
        assert_eq!(entries[0].reading.as_deref(), Some("しおや"));
    }

    #[test]
    fn full_width_comma_also_separates() {
        // 日本語入力では全角カンマの方が打ちやすい。
        let entries = parse_entries(&lines(&["塩谷，しおや"]));
        assert_eq!(entries[0].written, "塩谷");
        assert_eq!(entries[0].reading.as_deref(), Some("しおや"));
    }

    #[test]
    fn surrounding_space_is_trimmed() {
        let entries = parse_entries(&lines(&["  塩谷 , しおや  "]));
        assert_eq!(entries[0].written, "塩谷");
        assert_eq!(entries[0].reading.as_deref(), Some("しおや"));
    }

    #[test]
    fn blank_lines_and_empty_written_forms_are_dropped() {
        let entries = parse_entries(&lines(&["", "   ", ",よみだけ", "有効"]));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].written, "有効");
    }

    #[test]
    fn an_empty_reading_is_treated_as_absent() {
        let entries = parse_entries(&lines(&["表記,", "表記2,  "]));
        assert!(entries.iter().all(|e| e.reading.is_none()));
    }

    #[test]
    fn instruction_line_mentions_the_reading_only_when_present() {
        let with = DictionaryEntry { written: "塩谷".into(), reading: Some("しおや".into()) };
        assert!(with.as_instruction().contains("しおや"));
        assert!(with.as_instruction().contains("塩谷"));
        let without = DictionaryEntry { written: "Tauri".into(), reading: None };
        assert_eq!(without.as_instruction(), "Tauri");
    }

    // --- Whisper プロンプト ---

    #[test]
    fn no_dictionary_and_no_context_means_no_prompt() {
        assert_eq!(build_whisper_prompt(&[], None), None);
        assert_eq!(build_whisper_prompt(&[], Some("   ")), None);
    }

    #[test]
    fn dictionary_only_prompt_lists_the_terms() {
        let entries = parse_entries(&lines(&["nox-voice", "Tauri"]));
        assert_eq!(
            build_whisper_prompt(&entries, None).as_deref(),
            Some("nox-voice、Tauri")
        );
    }

    #[test]
    fn context_only_prompt_is_collapsed() {
        let prompt = build_whisper_prompt(&[], Some("会議の\n\n議事録   です")).expect("ある");
        assert_eq!(prompt, "会議の 議事録 です");
    }

    #[test]
    fn the_dictionary_goes_last_so_truncation_eats_the_context() {
        // API は超過分を先頭から捨てる。辞書を末尾に置くのが仕様。
        let entries = parse_entries(&lines(&["nox-voice"]));
        let prompt = build_whisper_prompt(&entries, Some("画面のテキスト")).expect("ある");
        assert!(prompt.ends_with("nox-voice"), "辞書が末尾に無い: {prompt}");
        assert!(prompt.contains("画面のテキスト"));
    }

    #[test]
    fn the_prompt_stays_within_the_budget() {
        let entries = parse_entries(&lines(&["用語"]));
        let long_context = "あ".repeat(1_000);
        let prompt = build_whisper_prompt(&entries, Some(&long_context)).expect("ある");
        assert!(
            prompt.chars().count() <= WHISPER_PROMPT_MAX_CHARS,
            "予算超過: {} 文字",
            prompt.chars().count()
        );
        // 溢れても辞書は残る。
        assert!(prompt.ends_with("用語"));
    }

    #[test]
    fn a_huge_dictionary_is_trimmed_at_term_boundaries() {
        // 語の途中で切れると別の語になってしまう。
        let terms: Vec<String> = (0..100).map(|i| format!("用語{i:03}")).collect();
        let entries = parse_entries(&terms);
        let prompt = build_whisper_prompt(&entries, None).expect("ある");
        assert!(prompt.chars().count() <= WHISPER_PROMPT_MAX_CHARS);
        for term in prompt.split('、') {
            assert!(
                terms.iter().any(|t| t == term),
                "語が途中で切れている: {term:?}"
            );
        }
    }

    #[test]
    fn a_term_over_budget_does_not_drop_the_rest() {
        // 長い語で打ち切ると、後ろの短い語が巻き添えで全滅する。
        let long = "あ".repeat(WHISPER_PROMPT_MAX_CHARS + 10);
        let entries = parse_entries(&[long, "短語".to_string(), "別語".to_string()]);
        let prompt = build_whisper_prompt(&entries, None).expect("ある");
        assert!(prompt.contains("短語"), "後続の語が落ちた: {prompt}");
        assert!(prompt.contains("別語"), "後続の語が落ちた: {prompt}");
        assert!(prompt.chars().count() <= WHISPER_PROMPT_MAX_CHARS);
    }

    #[test]
    fn a_single_oversized_term_yields_no_dictionary() {
        let long = "あ".repeat(WHISPER_PROMPT_MAX_CHARS + 10);
        let entries = parse_entries(&[long]);
        // 辞書が空でもコンテキストがあればプロンプトは作られる。
        let prompt = build_whisper_prompt(&entries, Some("画面テキスト")).expect("ある");
        assert_eq!(prompt, "画面テキスト");
        // 辞書もコンテキストも無ければ None。
        assert_eq!(build_whisper_prompt(&entries, None), None);
    }

    #[test]
    fn context_is_taken_from_the_end() {
        // キャレット付近 = 末尾の方が今の話題に近い。
        let context: String = (0..300).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let prompt = build_whisper_prompt(&[], Some(&context)).expect("ある");
        assert!(context.ends_with(&prompt), "末尾から取っていない");
    }

    #[test]
    fn a_dictionary_filling_the_budget_drops_the_context_entirely() {
        let terms: Vec<String> = (0..100).map(|i| format!("語{i:03}")).collect();
        let entries = parse_entries(&terms);
        let prompt = build_whisper_prompt(&entries, Some("画面テキスト")).expect("ある");
        assert!(!prompt.contains("画面テキスト"), "辞書より先にコンテキストが残った");
        assert!(prompt.chars().count() <= WHISPER_PROMPT_MAX_CHARS);
    }
}
