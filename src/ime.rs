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

    /// Abandons any in-progress composition (used when the editor
    /// switches into find/replace mode, where raw keys go to the
    /// search fields instead of the document).
    pub fn reset(&mut self) {
        self.composing = false;
        self.romaji.clear();
        self.candidates.clear();
        self.index = 0;
        self.inserted.clear();
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
    // ---- numbers ----
    ("いち", &["一", "位置"]),
    ("に", &["二", "煮"]),
    ("さん", &["三", "山"]),
    ("よん", &["四"]),
    ("ご", &["五", "午"]),
    ("ろく", &["六"]),
    ("なな", &["七"]),
    ("はち", &["八", "鉢"]),
    ("きゅう", &["九", "急", "休"]),
    ("じゅう", &["十"]),
    ("ひゃく", &["百"]),
    ("せん", &["千", "線", "先"]),
    ("まん", &["万"]),
    ("いちまん", &["一万"]),
    ("じゅういち", &["十一"]),
    ("にじゅう", &["二十"]),
    ("さんじゅう", &["三十"]),
    ("なんばん", &["何番"]),
    // ---- days / calendar ----
    ("いちにち", &["一日"]),
    ("ふつか", &["二日"]),
    ("みっか", &["三日"]),
    ("よっか", &["四日"]),
    ("いつか", &["五日"]),
    ("むいか", &["六日"]),
    ("なのか", &["七日"]),
    ("ようか", &["八日"]),
    ("ここのか", &["九日"]),
    ("とおか", &["十日"]),
    ("なんにち", &["何日"]),
    ("あさって", &["明後日"]),
    ("おととい", &["一昨日"]),
    ("こんしゅう", &["今週"]),
    ("らいしゅう", &["来週"]),
    ("せんしゅう", &["先週"]),
    ("こんげつ", &["今月"]),
    ("らいげつ", &["来月"]),
    ("せんげつ", &["先月"]),
    ("ことし", &["今年"]),
    ("らいねん", &["来年"]),
    ("きょねん", &["去年"]),
    // ---- time ----
    ("いま", &["今"]),
    ("ごぜん", &["午前"]),
    ("ごご", &["午後"]),
    ("あさ", &["朝"]),
    ("ひる", &["昼"]),
    ("よる", &["夜"]),
    ("ばん", &["晩"]),
    ("じ", &["時"]),
    ("ふん", &["分"]),
    ("びょう", &["秒"]),
    ("じこく", &["時刻"]),
    ("はん", &["半", "版", "判"]),
    ("すうじかん", &["数時間"]),
    // ---- people / family ----
    ("りょうしん", &["両親"]),
    ("ちち", &["父"]),
    ("はは", &["母"]),
    ("あに", &["兄"]),
    ("あね", &["姉"]),
    ("おとうと", &["弟"]),
    ("いもうと", &["妹"]),
    ("そふ", &["祖父"]),
    ("そぼ", &["祖母"]),
    ("おじ", &["叔父", "伯父"]),
    ("おば", &["叔母", "伯母"]),
    ("こども", &["子供"]),
    ("おとな", &["大人"]),
    ("おとこ", &["男"]),
    ("おんな", &["女"]),
    ("おとこのこ", &["男の子"]),
    ("おんなのこ", &["女の子"]),
    ("ひと", &["人"]),
    ("なかま", &["仲間"]),
    ("せんぱい", &["先輩"]),
    ("こうはい", &["後輩"]),
    ("しゃちょう", &["社長"]),
    ("いしゃ", &["医者"]),
    ("かいしゃいん", &["会社員"]),
    ("きゃく", &["客"]),
    ("しゅじん", &["主人"]),
    ("かない", &["家内"]),
    // ---- places ----
    ("としょかん", &["図書館"]),
    ("えき", &["駅"]),
    ("くうこう", &["空港"]),
    ("みなと", &["港"]),
    ("はし", &["橋"]),
    ("むら", &["村"]),
    ("くに", &["国"]),
    ("せかい", &["世界"]),
    ("ちゅうごく", &["中国"]),
    ("かんこく", &["韓国"]),
    ("アメリカ", &["アメリカ"]),
    ("いぎりす", &["イギリス"]),
    ("とうきょう", &["東京"]),
    ("おおさか", &["大阪"]),
    ("きょうと", &["京都"]),
    ("ほっかいどう", &["北海道"]),
    ("みずうみ", &["湖"]),
    ("くも", &["雲"]),
    ("あめ", &["雨", "飴"]),
    ("ゆき", &["雪"]),
    ("かぜ", &["風"]),
    ("かみなり", &["雷"]),
    ("たいよう", &["太陽"]),
    ("つき", &["月"]),
    ("ほし", &["星"]),
    // ---- nature / objects ----
    ("き", &["木", "気"]),
    ("は", &["葉", "歯"]),
    ("くさ", &["草"]),
    ("いし", &["石"]),
    ("つち", &["土"]),
    ("ひ", &["日", "火"]),
    ("かじ", &["火事"]),
    ("とけい", &["時計"]),
    ("かばん", &["鞄"]),
    ("ふく", &["服"]),
    ("くつ", &["靴"]),
    ("ぼうし", &["帽子"]),
    ("めがね", &["眼鏡"]),
    ("かさ", &["傘"]),
    ("えんぴつ", &["鉛筆"]),
    ("ぺん", &["ペン"]),
    ("かみ", &["紙", "髪"]),
    ("はこ", &["箱"]),
    ("かぎ", &["鍵"]),
    ("さいふ", &["財布"]),
    ("かべ", &["壁"]),
    ("まど", &["窓"]),
    ("とびら", &["扉"]),
    ("へや", &["部屋"]),
    ("いえ", &["家"]),
    ("たてもの", &["建物"]),
    // ---- food ----
    ("ごはん", &["ご飯"]),
    ("あさごはん", &["朝ご飯"]),
    ("ひるごはん", &["昼ご飯"]),
    ("ばんごはん", &["晩ご飯"]),
    ("たまご", &["卵"]),
    ("にく", &["肉"]),
    ("やさい", &["野菜"]),
    ("くだもの", &["果物"]),
    ("ばなな", &["バナナ"]),
    ("こうちゃ", &["紅茶"]),
    ("ぎゅうにゅう", &["牛乳"]),
    ("さとう", &["砂糖"]),
    ("しお", &["塩"]),
    ("ぱん", &["パン"]),
    ("めん", &["麺"]),
    ("すし", &["寿司"]),
    ("さしみ", &["刺身"]),
    ("てんぷら", &["天ぷら"]),
    ("おべんとう", &["お弁当"]),
    ("おかし", &["お菓子"]),
    ("けーき", &["ケーキ"]),
    // ---- school / work ----
    ("しゅくだい", &["宿題"]),
    ("てすと", &["テスト"]),
    ("しけん", &["試験"]),
    ("きょうかしょ", &["教科書"]),
    ("じゅぎょう", &["授業"]),
    ("こうぎ", &["講義"]),
    ("けんきゅう", &["研究"]),
    ("かいはつ", &["開発"]),
    ("せっけい", &["設計"]),
    ("ぷろぐらむ", &["プログラム"]),
    ("ぷろぐらみんぐ", &["プログラミング"]),
    ("こーど", &["コード"]),
    ("えらー", &["エラー"]),
    ("ばぐ", &["バグ"]),
    ("げんご", &["言語"]),
    ("かんじ", &["漢字"]),
    ("ひらがな", &["ひらがな"]),
    ("かたかな", &["カタカナ"]),
    ("ぶんしょう", &["文章"]),
    ("ぶんぽう", &["文法"]),
    ("すうじ", &["数字"]),
    ("けいさん", &["計算"]),
    ("しき", &["式"]),
    ("ちず", &["地図"]),
    ("つとめ", &["勤め"]),
    ("かいぎ", &["会議"]),
    ("ほうこく", &["報告"]),
    ("れんらく", &["連絡"]),
    ("よてい", &["予定"]),
    ("じゅんび", &["準備"]),
    ("かんり", &["管理"]),
    ("うんえい", &["運営"]),
    // ---- tech ----
    ("ぱそこん", &["パソコン"]),
    ("もにたー", &["モニター"]),
    ("ぷりんたー", &["プリンター"]),
    ("すくりーん", &["スクリーン"]),
    ("でぃすぷれい", &["ディスプレイ"]),
    ("がぞう", &["画像"]),
    ("どうが", &["動画"]),
    ("おんせい", &["音声"]),
    ("ぼたん", &["ボタン"]),
    ("めにゅー", &["メニュー"]),
    ("あいこん", &["アイコン"]),
    ("ふぉんと", &["フォント"]),
    ("めもり", &["メモリ"]),
    ("えいぞう", &["映像"]),
    ("がめん", &["画面"]),
    ("はいけい", &["背景"]),
    // ---- feelings ----
    ("うれしい", &["嬉しい"]),
    ("かなしい", &["悲しい"]),
    ("たのしい", &["楽しい"]),
    ("おもしろい", &["面白い"]),
    ("こわい", &["怖い"]),
    ("ねむい", &["眠い"]),
    ("つかれる", &["疲れる"]),
    ("きんちょう", &["緊張"]),
    ("しんぱい", &["心配"]),
    ("あんしん", &["安心"]),
    ("きぶん", &["気分"]),
    ("げんき", &["元気"]),
    ("すき", &["好き"]),
    ("きらい", &["嫌い"]),
    // ---- adjectives ----
    ("おおきい", &["大きい"]),
    ("ちいさい", &["小さい"]),
    ("たかい", &["高い"]),
    ("やすい", &["安い"]),
    ("ひくい", &["低い"]),
    ("あたらしい", &["新しい"]),
    ("ふるい", &["古い"]),
    ("ながい", &["長い"]),
    ("みじかい", &["短い"]),
    ("はやい", &["早い", "速い"]),
    ("おそい", &["遅い"]),
    ("あつい", &["暑い", "熱い", "厚い"]),
    ("さむい", &["寒い"]),
    ("つめたい", &["冷たい"]),
    ("あたたかい", &["暖かい"]),
    ("すずしい", &["涼しい"]),
    ("おいしい", &["美味しい"]),
    ("うつくしい", &["美しい"]),
    ("かわいい", &["可愛い"]),
    ("きれい", &["綺麗"]),
    ("ただしい", &["正しい"]),
    ("むずかしい", &["難しい"]),
    ("やさしい", &["優しい", "易しい"]),
    ("かんたん", &["簡単"]),
    ("ふくざつ", &["複雑"]),
    ("じゆう", &["自由"]),
    ("ひつよう", &["必要"]),
    ("たいせつ", &["大切"]),
    ("だいじょうぶ", &["大丈夫"]),
    // ---- verbs ----
    ("たべる", &["食べる"]),
    ("のむ", &["飲む"]),
    ("みる", &["見る"]),
    ("きく", &["聞く", "効く"]),
    ("はなす", &["話す"]),
    ("よむ", &["読む"]),
    ("かく", &["書く", "描く"]),
    ("いく", &["行く"]),
    ("くる", &["来る"]),
    ("かえる", &["帰る", "返る"]),
    ("おきる", &["起きる"]),
    ("ねる", &["寝る"]),
    ("あそぶ", &["遊ぶ"]),
    ("はたらく", &["働く"]),
    ("つくる", &["作る", "造る"]),
    ("つかう", &["使う"]),
    ("まつ", &["待つ"]),
    ("もつ", &["持つ"]),
    ("しる", &["知る"]),
    ("わかる", &["分かる"]),
    ("おもう", &["思う"]),
    ("いう", &["言う"]),
    ("あう", &["会う"]),
    ("かう", &["買う"]),
    ("うる", &["売る"]),
    ("まなぶ", &["学ぶ"]),
    ("ならう", &["習う"]),
    ("かよう", &["通う"]),
    ("すむ", &["住む"]),
    ("あく", &["開く"]),
    ("しめる", &["閉める"]),
    ("あける", &["開ける"]),
    ("なおす", &["直す", "治す"]),
    ("けす", &["消す"]),
    ("つける", &["付ける"]),
    ("おくる", &["送る"]),
    ("でる", &["出る"]),
    ("はいる", &["入る"]),
    ("でかける", &["出掛ける"]),
    ("やすむ", &["休む"]),
    ("わたる", &["渡る"]),
    ("あるく", &["歩く"]),
    ("はしる", &["走る"]),
    ("とぶ", &["飛ぶ"]),
    ("およぐ", &["泳ぐ"]),
    ("うたう", &["歌う"]),
    ("おどる", &["踊る"]),
    // ---- misc ----
    ("ことば", &["言葉"]),
    ("いみ", &["意味"]),
    ("なまえ", &["名前"]),
    ("あいさつ", &["挨拶"]),
    ("じこしょうかい", &["自己紹介"]),
    ("てがみ", &["手紙"]),
    ("はがき", &["葉書"]),
    ("きって", &["切手"]),
    ("ゆうびん", &["郵便"]),
    ("ゆうびんきょく", &["郵便局"]),
    ("ぎんこう", &["銀行"]),
    ("こうばん", &["交番"]),
    ("けいさつ", &["警察"]),
    ("しょうぼう", &["消防"]),
    ("きゅうきゅうしゃ", &["救急車"]),
    ("ちかてつ", &["地下鉄"]),
    ("ばす", &["バス"]),
    ("たくしー", &["タクシー"]),
    ("どうろ", &["道路"]),
    ("こうさてん", &["交差点"]),
    ("しんごう", &["信号"]),
    ("てんきよほう", &["天気予報"]),
    ("うんどう", &["運動"]),
    ("しゅみ", &["趣味"]),
    ("ぼく", &["僕"]),
    ("わたしたち", &["私たち"]),
    ("みなさん", &["皆さん"]),
    ("かた", &["方"]),
    ("ところ", &["所"]),
    ("ばしょ", &["場所"]),
    ("ものがたり", &["物語"]),
    ("うし", &["牛"]),
    ("うま", &["馬"]),
    ("ぶた", &["豚"]),
    ("ひつじ", &["羊"]),
    ("さる", &["猿"]),
    ("むし", &["虫"]),
    ("えび", &["海老"]),
    ("かに", &["蟹"]),
];
