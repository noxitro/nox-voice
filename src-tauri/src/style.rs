//! アプリ別の整形スタイル。
//!
//! 同じ発話でも、Slack に書くのとメールに書くのでは望ましい文体が違う。
//! 挿入先のプロセス名 (必要ならウィンドウタイトル) でプロファイルを選び、
//! 整形プロンプトへスタイル指示として差し込む。
//!
//! ブラウザのように 1 つのプロセスで用途が変わるものは、タイトルの
//! 部分一致 (`title_contains`) で絞り込める。

use serde::{Deserialize, Serialize};

/// 1 つのスタイルプロファイル。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StyleProfile {
    /// プロセス名の一致条件 (大文字小文字を無視した部分一致)。
    /// 例: `slack.exe`、`chrome`
    pub process: String,
    /// ウィンドウタイトルの追加条件 (部分一致)。ブラウザの用途分けに使う。
    #[serde(default)]
    pub title_contains: Option<String>,
    /// 整形へ渡すスタイル指示。
    pub instruction: String,
}

impl StyleProfile {
    fn matches(&self, process: &str, title: &str) -> bool {
        let process_pattern = self.process.trim();
        if process_pattern.is_empty() {
            return false;
        }
        if !contains_ignore_case(process, process_pattern) {
            return false;
        }
        match self.title_contains.as_deref().map(str::trim) {
            Some(needle) if !needle.is_empty() => contains_ignore_case(title, needle),
            _ => true,
        }
    }

    /// 条件の細かさ。同点の場合により具体的な方を採る。
    fn specificity(&self) -> usize {
        let title_bonus = match self.title_contains.as_deref().map(str::trim) {
            Some(t) if !t.is_empty() => 1_000,
            _ => 0,
        };
        title_bonus + self.process.trim().chars().count()
    }
}

fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

/// 挿入先に合うプロファイルを選ぶ。
///
/// 複数当てはまる場合は**より具体的な方**を採る
/// (タイトル条件つき > プロセス名が長い)。同点なら先に定義された方。
/// これにより、汎用の `chrome` プロファイルを置いたうえで
/// `chrome` + `Gmail` の特例を足す、という書き方ができる。
pub fn match_profile<'a>(
    profiles: &'a [StyleProfile],
    process: &str,
    title: &str,
) -> Option<&'a StyleProfile> {
    // `max_by_key` は同点で**最後**を返すので使わない。
    // 「同点なら先に定義された方」を守るため、真に上回ったときだけ差し替える。
    profiles
        .iter()
        .filter(|p| p.matches(process, title))
        .fold(None, |best: Option<&'a StyleProfile>, candidate| match best {
            Some(current) if current.specificity() >= candidate.specificity() => Some(current),
            _ => Some(candidate),
        })
}

/// 同梱する既定プロファイル。
///
/// ユーザーが設定を書かなくても、よく使うアプリで文体が合うようにする。
/// 設定 UI から編集・削除できる。
pub fn default_profiles() -> Vec<StyleProfile> {
    let make = |process: &str, instruction: &str| StyleProfile {
        process: process.to_string(),
        title_contains: None,
        instruction: instruction.to_string(),
    };
    vec![
        make(
            "slack.exe",
            "チャットの発言。簡潔な口語で、丁寧すぎない自然な調子にする。挨拶や定型の前置きは付けない",
        ),
        make(
            "discord.exe",
            "チャットの発言。簡潔な口語で、丁寧すぎない自然な調子にする",
        ),
        make(
            "teams.exe",
            "社内チャットの発言。簡潔だが失礼にならない程度の丁寧さを保つ",
        ),
        make(
            "outlook.exe",
            "メール本文。ですます調の丁寧な文体にし、文の区切りで改行を入れる。宛名や署名は追加しない",
        ),
        make(
            "code.exe",
            "技術的な文章。専門用語・製品名・コード片は原形のまま保ち、勝手に言い換えない",
        ),
        make(
            "devenv.exe",
            "技術的な文章。専門用語・製品名・コード片は原形のまま保ち、勝手に言い換えない",
        ),
        make(
            "windowsterminal.exe",
            "コマンドやコード片。日本語の句読点を足さず、入力された記号や英数字をそのまま保つ",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(process: &str, title: Option<&str>, instruction: &str) -> StyleProfile {
        StyleProfile {
            process: process.to_string(),
            title_contains: title.map(str::to_string),
            instruction: instruction.to_string(),
        }
    }

    #[test]
    fn matches_the_process_name_ignoring_case() {
        let profiles = vec![profile("slack.exe", None, "カジュアル")];
        let hit = match_profile(&profiles, "Slack.EXE", "").expect("一致する");
        assert_eq!(hit.instruction, "カジュアル");
    }

    #[test]
    fn matches_on_a_partial_process_name() {
        // 実行ファイル名が環境で変わるアプリのために部分一致にしてある。
        let profiles = vec![profile("chrome", None, "ブラウザ")];
        assert!(match_profile(&profiles, "chrome.exe", "").is_some());
        assert!(match_profile(&profiles, "GoogleChrome.exe", "").is_some());
    }

    #[test]
    fn a_non_matching_process_selects_nothing() {
        let profiles = vec![profile("slack.exe", None, "カジュアル")];
        assert!(match_profile(&profiles, "notepad.exe", "").is_none());
    }

    #[test]
    fn a_title_condition_narrows_the_match() {
        let profiles = vec![profile("chrome", Some("Gmail"), "メール")];
        assert!(match_profile(&profiles, "chrome.exe", "受信トレイ - Gmail").is_some());
        assert!(
            match_profile(&profiles, "chrome.exe", "ニュースサイト").is_none(),
            "タイトル条件が効いていない"
        );
    }

    #[test]
    fn the_more_specific_profile_wins() {
        // 汎用のブラウザ設定に、Gmail だけの特例を足せること。
        let profiles = vec![
            profile("chrome", None, "汎用ブラウザ"),
            profile("chrome", Some("Gmail"), "メール"),
        ];
        let hit = match_profile(&profiles, "chrome.exe", "受信トレイ - Gmail").expect("一致");
        assert_eq!(hit.instruction, "メール", "特例より汎用が勝ってしまった");

        let hit = match_profile(&profiles, "chrome.exe", "ニュース").expect("一致");
        assert_eq!(hit.instruction, "汎用ブラウザ");
    }

    #[test]
    fn a_longer_process_pattern_beats_a_shorter_one() {
        let profiles = vec![
            profile("code", None, "汎用エディタ"),
            profile("code.exe", None, "VS Code"),
        ];
        let hit = match_profile(&profiles, "code.exe", "").expect("一致");
        assert_eq!(hit.instruction, "VS Code");
    }

    #[test]
    fn ties_go_to_the_first_definition() {
        // doc の約束どおり、同じ具体性なら先に書いた方が勝つ。
        let profiles = vec![
            profile("slack.exe", None, "先に定義"),
            profile("slack.exe", None, "後に定義"),
        ];
        let hit = match_profile(&profiles, "slack.exe", "").expect("一致");
        assert_eq!(hit.instruction, "先に定義");
    }

    #[test]
    fn ties_with_title_conditions_also_go_to_the_first() {
        let profiles = vec![
            profile("chrome", Some("Gmail"), "先に定義"),
            profile("chrome", Some("Gmail"), "後に定義"),
        ];
        let hit = match_profile(&profiles, "chrome.exe", "受信トレイ - Gmail").expect("一致");
        assert_eq!(hit.instruction, "先に定義");
    }

    #[test]
    fn an_empty_process_pattern_never_matches() {
        // 空欄を「全部に当てはまる」と解釈すると、設定の書きかけが
        // すべての発話に効いてしまう。
        let profiles = vec![profile("   ", None, "壊れた設定")];
        assert!(match_profile(&profiles, "notepad.exe", "何か").is_none());
    }

    #[test]
    fn an_empty_title_condition_is_ignored() {
        let profiles = vec![profile("slack.exe", Some("  "), "カジュアル")];
        assert!(match_profile(&profiles, "slack.exe", "").is_some());
    }

    #[test]
    fn unknown_target_falls_through_to_no_style() {
        // 既定プロファイルに無いアプリでは、スタイル指示なしで整形する。
        assert!(match_profile(&default_profiles(), "notepad.exe", "無題").is_none());
    }

    #[test]
    fn bundled_profiles_cover_the_common_cases() {
        let profiles = default_profiles();
        let slack = match_profile(&profiles, "slack.exe", "").expect("Slack");
        assert!(slack.instruction.contains("口語"));
        let outlook = match_profile(&profiles, "OUTLOOK.EXE", "").expect("Outlook");
        assert!(outlook.instruction.contains("ですます"));
        let editor = match_profile(&profiles, "Code.exe", "").expect("エディタ");
        assert!(editor.instruction.contains("専門用語"));
    }

    #[test]
    fn bundled_profiles_are_serializable() {
        // 設定ファイルへ往復できること (UI から編集するため)。
        let json = serde_json::to_string(&default_profiles()).expect("直列化");
        let back: Vec<StyleProfile> = serde_json::from_str(&json).expect("復元");
        assert_eq!(back, default_profiles());
    }

    #[test]
    fn a_profile_without_title_field_deserializes() {
        // 手書きの設定で title_contains を省略できること。
        let json = r#"[{"process":"slack.exe","instruction":"カジュアル"}]"#;
        let profiles: Vec<StyleProfile> = serde_json::from_str(json).expect("復元");
        assert_eq!(profiles[0].title_contains, None);
    }
}
