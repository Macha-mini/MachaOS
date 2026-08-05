//! A minimal Japanese IME: romaji -> kana conversion with a small
//! kanji dictionary, integrated at the window-manager key-dispatch
//! level for the Notepad editor.
//!
//! Model: while composing, the romaji the user types is passed through
//! to the editor as ordinary characters, so it is visible in the text
//! immediately. Space then converts: the IME emits `Backspace` events
//! to erase the romaji and `Char` events to insert the converted
//! string. When a reading has several candidates, further Spaces cycle
//! through them (kana is always the last candidate), number keys pick
//! one directly, and Enter commits. Any other key commits whatever is
//! on screen and then passes through normally.
//!
//! Limitation: conversion assumes the cursor still sits right after the
//! romaji that was typed (moving the cursor mid-composition desyncs the
//! backspace count). Acceptable for v1.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::keyboard;

/// The events a `feed` call wants delivered to the editor, plus an
/// optional status-bar message.
pub struct ImeOutcome {
    pub events: Vec<keyboard::Event>,
    pub status: Option<String>,
}

pub struct Ime {
    composing: bool,
    /// Romaji typed during the current composition (mirrors what is in
    /// the document, so backspace counts stay in sync).
    romaji: String,
    /// Candidate strings for the current reading; empty before the
    /// first conversion.
    candidates: Vec<String>,
    index: usize,
    /// The candidate most recently inserted by a conversion (erased
    /// first when cycling to the next one).
    inserted: String,
}

impl Ime {
    pub const fn new() -> Self {
        Self {
            composing: false,
            romaji: String::new(),
            candidates: Vec::new(),
            index: 0,
            inserted: String::new(),
        }
    }

    pub fn feed(&mut self, event: keyboard::Event) -> ImeOutcome {
        match event {
            keyboard::Event::Char(c) if c.is_ascii_lowercase() => {
                if !self.composing {
                    self.composing = true;
                    self.romaji.clear();
                    self.candidates.clear();
                    self.inserted.clear();
                } else if !self.candidates.is_empty() {
                    // A conversion is showing; the new letter commits it
                    // and starts a fresh composition.
                    self.candidates.clear();
                    self.inserted.clear();
                    self.romaji.clear();
                }
                self.romaji.push(c);
                ImeOutcome {
                    events: vec![event],
                    status: self.preview_status(),
                }
            }
            keyboard::Event::Char(' ') if self.composing => {
                if self.candidates.is_empty() {
                    self.first_conversion()
                } else {
                    self.cycle_candidate()
                }
            }
            keyboard::Event::Char(c) if self.composing && c.is_ascii_digit() => {
                self.select_candidate(c)
            }
            keyboard::Event::Enter if self.composing => {
                // Commit whatever is on screen (romaji or a candidate).
                self.composing = false;
                self.romaji.clear();
                self.candidates.clear();
                self.inserted.clear();
                ImeOutcome {
                    events: vec![event],
                    status: None,
                }
            }
            keyboard::Event::Backspace if self.composing => {
                self.romaji.pop();
                if self.romaji.is_empty() {
                    self.composing = false;
                }
                ImeOutcome {
                    events: vec![event],
                    status: self.preview_status(),
                }
            }
            ev => {
                // Any other key commits the composition first.
                if self.composing {
                    self.composing = false;
                    self.romaji.clear();
                    self.candidates.clear();
                    self.inserted.clear();
                }
                ImeOutcome {
                    events: vec![ev],
                    status: None,
                }
            }
        }
    }

    /// Space on a fresh reading: romaji -> kana, look up kanji
    /// candidates, insert the first one and keep composing so more
    /// Spaces can cycle.
    fn first_conversion(&mut self) -> ImeOutcome {
        let kana = romaji_to_kana(&self.romaji);
        let mut candidates: Vec<String> = DICT
            .iter()
            .find(|(reading, _)| *reading == kana)
            .map(|(_, cs)| cs.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default();
        candidates.push(kana); // kana is always the last resort
        self.candidates = candidates;
        self.index = 0;
        let mut events = Vec::new();
        for _ in 0..self.romaji.chars().count() {
            events.push(keyboard::Event::Backspace);
        }
        let first = self.candidates[0].clone();
        for ch in first.chars() {
            events.push(keyboard::Event::Char(ch));
        }
        self.inserted = first;
        self.romaji.clear();
        // Nothing to cycle through -> commit immediately.
        if self.candidates.len() == 1 {
            self.composing = false;
            self.candidates.clear();
            self.inserted.clear();
            ImeOutcome {
                events,
                status: None,
            }
        } else {
            ImeOutcome {
                events,
                status: self.candidate_status(),
            }
        }
    }

    /// Space while candidates are showing: move to the next one,
    /// erasing the previous insertion first.
    fn cycle_candidate(&mut self) -> ImeOutcome {
        self.index = (self.index + 1) % self.candidates.len();
        let mut events = Vec::new();
        for _ in 0..self.inserted.chars().count() {
            events.push(keyboard::Event::Backspace);
        }
        let next = self.candidates[self.index].clone();
        for ch in next.chars() {
            events.push(keyboard::Event::Char(ch));
        }
        self.inserted = next;
        ImeOutcome {
            events,
            status: self.candidate_status(),
        }
    }

    /// Number key 1..=9 selects a candidate directly and commits.
    fn select_candidate(&mut self, c: char) -> ImeOutcome {
        let n = (c as usize) - ('1' as usize) + 1;
        if n <= self.candidates.len() {
            self.index = n - 1;
            let mut events = Vec::new();
            for _ in 0..self.inserted.chars().count() {
                events.push(keyboard::Event::Backspace);
            }
            let chosen = self.candidates[self.index].clone();
            for ch in chosen.chars() {
                events.push(keyboard::Event::Char(ch));
            }
            self.composing = false;
            self.candidates.clear();
            self.inserted.clear();
            ImeOutcome {
                events,
                status: None,
            }
        } else {
            // Out of range: commit the current candidate and pass the
            // digit through.
            self.composing = false;
            self.candidates.clear();
            self.inserted.clear();
            ImeOutcome {
                events: vec![keyboard::Event::Char(c)],
                status: None,
            }
        }
    }

    /// Status while typing romaji: show the kana it will convert to.
    fn preview_status(&self) -> Option<String> {
        if self.composing && !self.romaji.is_empty() {
            Some(format!(
                "IME: {} (Spaceで変換)",
                romaji_to_kana(&self.romaji)
            ))
        } else {
            None
        }
    }

    /// Status while cycling candidates: current one bracketed.
    fn candidate_status(&self) -> Option<String> {
        if self.candidates.is_empty() {
            return None;
        }
        let mut shown = String::new();
        for (i, c) in self.candidates.iter().enumerate() {
            if i > 0 {
                shown.push(' ');
            }
            if i == self.index {
                shown.push('[');
                shown.push_str(c);
                shown.push(']');
            } else {
                shown.push_str(c);
            }
        }
        Some(format!(
            "変換: {} (Space: 次 / 数字: 選択 / Enter: 確定)",
            shown
        ))
    }
}

/// Romaji -> hiragana, longest-match greedy. Handles the common rules:
/// digraphs (kya -> きゃ), sokuon (kka -> っか), and the n-rules
/// (kan -> かん, kann -> かん, minna -> みんな).
fn romaji_to_kana(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        // Sokuon: doubled consonant -> small tsu.
        if i + 1 < chars.len()
            && chars[i + 1] == c
            && matches!(c, 'k' | 's' | 't' | 'p' | 'g' | 'z' | 'd' | 'b')
        {
            out.push('っ');
            i += 1;
            continue;
        }
        if c == 'n' {
            let nxt = chars.get(i + 1).copied();
            let nxt2 = chars.get(i + 2).copied();
            match nxt {
                None => {
                    out.push('ん');
                    i += 1;
                    continue;
                }
                Some('n') => match nxt2 {
                    None => {
                        // "kann" -> かん
                        out.push('ん');
                        i += 2;
                        continue;
                    }
                    Some(nc) if !is_vowel(nc) => {
                        // "kannto" -> かんと
                        out.push('ん');
                        i += 2;
                        continue;
                    }
                    // "minna": ん + na-row continues below.
                    _ => {
                        out.push('ん');
                        i += 1;
                        continue;
                    }
                },
                Some(nc) if !is_vowel(nc) && nc != 'y' => {
                    // n before a consonant -> ん
                    out.push('ん');
                    i += 1;
                    continue;
                }
                _ => {} // n before vowel/y: table handles na/nya...
            }
        }
        let mut matched = false;
        for len in [3usize, 2, 1] {
            if i + len <= chars.len() {
                let sub: String = chars[i..i + len].iter().collect();
                if let Some(k) = KANA_TABLE.iter().find(|(r, _)| *r == sub).map(|(_, k)| *k) {
                    out.push_str(k);
                    i += len;
                    matched = true;
                    break;
                }
            }
        }
        if !matched {
            // Unknown input (e.g. an apostrophe): keep it as-is.
            out.push(c);
            i += 1;
        }
    }
    out
}

fn is_vowel(c: char) -> bool {
    matches!(c, 'a' | 'i' | 'u' | 'e' | 'o')
}

/// Greedy romaji -> kana table. 3-char entries first (digraphs and
/// special spellings), then 2-char, then single vowels.
const KANA_TABLE: &[(&str, &str)] = &[
    ("kya", "きゃ"), ("kyu", "きゅ"), ("kyo", "きょ"),
    ("sha", "しゃ"), ("shu", "しゅ"), ("sho", "しょ"),
    ("cha", "ちゃ"), ("chu", "ちゅ"), ("cho", "ちょ"),
    ("nya", "にゃ"), ("nyu", "にゅ"), ("nyo", "にょ"),
    ("hya", "ひゃ"), ("hyu", "ひゅ"), ("hyo", "ひょ"),
    ("mya", "みゃ"), ("myu", "みゅ"), ("myo", "みょ"),
    ("rya", "りゃ"), ("ryu", "りゅ"), ("ryo", "りょ"),
    ("gya", "ぎゃ"), ("gyu", "ぎゅ"), ("gyo", "ぎょ"),
    ("bya", "びゃ"), ("byu", "びゅ"), ("byo", "びょ"),
    ("pya", "ぴゃ"), ("pyu", "ぴゅ"), ("pyo", "ぴょ"),
    ("shi", "し"), ("chi", "ち"), ("tsu", "つ"),
    ("ka", "か"), ("ki", "き"), ("ku", "く"), ("ke", "け"), ("ko", "こ"),
    ("sa", "さ"), ("su", "す"), ("se", "せ"), ("so", "そ"),
    ("ta", "た"), ("te", "て"), ("to", "と"),
    ("na", "な"), ("ni", "に"), ("nu", "ぬ"), ("ne", "ね"), ("no", "の"),
    ("ha", "は"), ("hi", "ひ"), ("he", "へ"), ("ho", "ほ"),
    ("fu", "ふ"),
    ("ma", "ま"), ("mi", "み"), ("mu", "む"), ("me", "め"), ("mo", "も"),
    ("ya", "や"), ("yu", "ゆ"), ("yo", "よ"),
    ("ra", "ら"), ("ri", "り"), ("ru", "る"), ("re", "れ"), ("ro", "ろ"),
    ("wa", "わ"), ("wo", "を"),
    ("ga", "が"), ("gi", "ぎ"), ("gu", "ぐ"), ("ge", "げ"), ("go", "ご"),
    ("za", "ざ"), ("ji", "じ"), ("zu", "ず"), ("ze", "ぜ"), ("zo", "ぞ"),
    ("da", "だ"), ("di", "ぢ"), ("du", "づ"), ("de", "で"), ("do", "ど"),
    ("ba", "ば"), ("bi", "び"), ("bu", "ぶ"), ("be", "べ"), ("bo", "ぼ"),
    ("pa", "ぱ"), ("pi", "ぴ"), ("pu", "ぷ"), ("pe", "ぺ"), ("po", "ぽ"),
    ("a", "あ"), ("i", "い"), ("u", "う"), ("e", "え"), ("o", "お"),
];

/// Small everyday kanji dictionary: hiragana reading -> candidate
/// strings. The kana reading itself is always appended as the last
/// candidate by the converter, so it is not listed here.
const DICT: &[(&str, &[&str])] = &[
    ("にほん", &["日本", "二本"]),
    ("にほんご", &["日本語"]),
    ("わたし", &["私"]),
    ("がっこう", &["学校"]),
    ("がくせい", &["学生"]),
    ("せんせい", &["先生"]),
    ("ともだち", &["友達"]),
    ("かぞく", &["家族"]),
    ("でんわ", &["電話"]),
    ("てんき", &["天気"]),
    ("しんぶん", &["新聞"]),
    ("ざっし", &["雑誌"]),
    ("ほん", &["本"]),
    ("じしょ", &["辞書"]),
    ("かいしゃ", &["会社"]),
    ("しごと", &["仕事"]),
    ("やすみ", &["休み"]),
    ("きょう", &["今日"]),
    ("あした", &["明日"]),
    ("きのう", &["昨日"]),
    ("すうがく", &["数学"]),
    ("えいご", &["英語"]),
    ("でんき", &["電気"]),
    ("みず", &["水"]),
    ("おちゃ", &["お茶"]),
    ("たべもの", &["食べ物"]),
    ("のみもの", &["飲み物"]),
    ("さかな", &["魚"]),
    ("いぬ", &["犬"]),
    ("ねこ", &["猫"]),
    ("とり", &["鳥"]),
    ("はな", &["花"]),
    ("そら", &["空"]),
    ("うみ", &["海"]),
    ("やま", &["山"]),
    ("かわ", &["川"]),
    ("まち", &["町"]),
    ("みち", &["道"]),
    ("でんしゃ", &["電車"]),
    ("くるま", &["車"]),
    ("じてんしゃ", &["自転車"]),
    ("ひこうき", &["飛行機"]),
    ("ふね", &["船"]),
    ("こうえん", &["公園"]),
    ("びょういん", &["病院"]),
    ("くすり", &["薬"]),
    ("おかね", &["お金"]),
    ("かいもの", &["買い物"]),
    ("りょこう", &["旅行"]),
    ("しゃしん", &["写真"]),
    ("おんがく", &["音楽"]),
    ("えいが", &["映画"]),
    ("てれび", &["テレビ"]),
    ("こんぴゅーたー", &["コンピューター"]),
    ("いんたーねっと", &["インターネット"]),
    ("けいたいでんわ", &["携帯電話"]),
    ("めーる", &["メール"]),
    ("きーぼーど", &["キーボード"]),
    ("まうす", &["マウス"]),
    ("ふぁいる", &["ファイル"]),
    ("ふぉるだー", &["フォルダー"]),
    ("でーた", &["データ"]),
    ("おす", &["OS"]),
    ("うぃんどう", &["ウィンドウ"]),
    ("ですくとっぷ", &["デスクトップ"]),
    ("しすてむ", &["システム"]),
    ("せってい", &["設定"]),
    ("ぶんしょ", &["文書"]),
    ("きろく", &["記録"]),
    ("ほぞん", &["保存"]),
    ("さくじょ", &["削除"]),
    ("いどう", &["移動"]),
    ("こぴー", &["コピー"]),
    ("ぺーすと", &["ペースト"]),
    ("いちらん", &["一覧"]),
    ("ひょうじ", &["表示"]),
    ("とうろく", &["登録"]),
    ("ほんやく", &["翻訳"]),
    ("まいにち", &["毎日"]),
    ("げつようび", &["月曜日"]),
    ("かようび", &["火曜日"]),
    ("すいようび", &["水曜日"]),
    ("もくようび", &["木曜日"]),
    ("きんようび", &["金曜日"]),
    ("どようび", &["土曜日"]),
    ("にちようび", &["日曜日"]),
    ("じかん", &["時間"]),
    ("こんばんは", &["こんばんは"]),
    ("おはよう", &["おはよう"]),
];
