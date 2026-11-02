//! 字幕の文字列の扱い: 表示幅、禁則と句読点を考えた改行、重複判定の類似度。
//!
//! 幅は「半角 = 1、全角 = 2」の単位で数える。1 行 32 全角文字なら上限は 64。

/// 1 文字の表示幅（半角 1、全角 2）。East Asian Width の W・F をおおまかに 2 とする。
pub fn char_width(c: char) -> u32 {
    let u = c as u32;
    let wide = matches!(u,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD);
    if wide { 2 } else { 1 }
}

/// 文字列の表示幅。
pub fn text_width(s: &str) -> u32 {
    s.chars().map(char_width).sum()
}

/// 行頭に置かない文字（行頭禁則）。
const NO_START: &str = "、。，．,.!?！？)）]］}｝」』】〉》〕ゝゞーァィゥェォッャュョヮヵヶぁぃぅぇぉっゃゅょゎゕゖ・：；:;…‥～〜%％";
/// 行末に置かない文字（行末禁則）。
const NO_END: &str = "(（[［{｛「『【〈《〔";
/// 文の終わり。
const SENTENCE_END: &str = "。．！？!?";
/// 句の切れ目。
const CLAUSE_END: &str = "、，,;；:：」』）)】";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Prio {
    /// 文字の間（全角の文字の間）。
    Char = 1,
    /// 空白。
    Space = 2,
    /// 句読点の後。
    Clause = 3,
    /// 文の終わりの後。
    Sentence = 4,
}

/// `chars[i-1]` と `chars[i]` の間で改行してよいか。
fn break_prio(prev: char, next: char) -> Option<Prio> {
    if next.is_whitespace() {
        return None;
    }
    if prev.is_whitespace() {
        return Some(Prio::Space);
    }
    if NO_START.contains(next) || NO_END.contains(prev) {
        return None;
    }
    if SENTENCE_END.contains(prev) {
        return Some(Prio::Sentence);
    }
    if CLAUSE_END.contains(prev) {
        return Some(Prio::Clause);
    }
    if char_width(prev) == 2 || char_width(next) == 2 {
        return Some(Prio::Char);
    }
    None
}

/// 行に分ける。各行の幅は `max_width` 以下（1 文字で超える場合と、切れ目のない長い英単語は例外）。
///
/// 切る位置は、行の幅の 4 割以上を使える範囲で「文の終わり > 句読点 > 空白 > 全角文字の間」の順に選び、
/// 同じ強さなら後ろを選ぶ。行頭・行末の禁則を守る。
pub fn wrap_lines(text: &str, max_width: u32) -> Vec<String> {
    let max_width = max_width.max(2);
    let chars: Vec<char> = normalize_spaces(text).chars().collect();
    let mut lines = Vec::new();
    let mut s = 0;
    while s < chars.len() {
        let mut w = 0u32;
        let mut i = s;
        // 幅に収まる最後の位置
        while i < chars.len() && w + char_width(chars[i]) <= max_width {
            w += char_width(chars[i]);
            i += 1;
        }
        if i >= chars.len() {
            push_line(&mut lines, &chars[s..]);
            break;
        }
        // 切れ目を探す（s+1..=i の位置 b で chars[b-1] | chars[b]）
        let min_w = max_width * 4 / 10;
        let mut best: Option<(Prio, usize)> = None;
        let mut fallback: Option<usize> = None;
        let mut acc = 0u32;
        for b in s + 1..=i {
            acc += char_width(chars[b - 1]);
            if let Some(p) = break_prio(chars[b - 1], chars[b]) {
                fallback = Some(b);
                if acc >= min_w && best.is_none_or(|(bp, _)| p >= bp) {
                    best = Some((p, b));
                }
            }
        }
        let cut = match (best, fallback) {
            (Some((_, b)), _) => b,
            (None, Some(b)) => b,
            (None, None) => {
                // 切れ目がない（長い英単語など）。次の空白まで延ばす。それもなければ幅で切る
                let mut j = i;
                while j < chars.len() && !chars[j].is_whitespace() {
                    j += 1;
                }
                if j - s > 3 * max_width as usize { i.max(s + 1) } else { j.max(s + 1) }
            }
        };
        push_line(&mut lines, &chars[s..cut]);
        s = cut;
        while s < chars.len() && chars[s].is_whitespace() {
            s += 1;
        }
    }
    lines
}

fn push_line(lines: &mut Vec<String>, chars: &[char]) {
    let l: String = chars.iter().collect::<String>().trim().to_string();
    if !l.is_empty() {
        lines.push(l);
    }
}

/// 改行・連続する空白を 1 つの空白にし、制御文字を除く。
pub fn normalize_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
        } else if !c.is_control() {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(c);
        }
    }
    out
}

/// 画面に出す単位（ページ）に分ける。1 ページは `max_lines` 行まで、行は `\n` でつなぐ。
pub fn split_pages(text: &str, max_width: u32, max_lines: usize) -> Vec<String> {
    let lines = wrap_lines(text, max_width);
    lines.chunks(max_lines.max(1)).map(|c| c.join("\n")).collect()
}

/// 比べるための正規化（空白・句読点・記号を除き、英字は小文字、全角英数は半角に）。
pub fn normalize_for_compare(s: &str) -> Vec<char> {
    s.chars()
        .filter_map(|c| {
            let c = match c as u32 {
                0xFF01..=0xFF5E => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
                _ => c,
            };
            if c.is_alphanumeric() { Some(c.to_lowercase().next().unwrap_or(c)) } else { None }
        })
        .collect()
}

/// 2 つの文字列の似ている度合い（0〜1）。文字の 2-gram で、Dice 係数と「短い方がどれだけ長い方に含まれるか」の大きい方。
pub fn similarity(a: &str, b: &str) -> f32 {
    let a = normalize_for_compare(a);
    let b = normalize_for_compare(b);
    if a.is_empty() || b.is_empty() {
        return if a.is_empty() && b.is_empty() { 1.0 } else { 0.0 };
    }
    if a.len() < 2 || b.len() < 2 {
        let (s, l) = if a.len() <= b.len() { (&a, &b) } else { (&b, &a) };
        return if l.contains(&s[0]) { 1.0 / l.len() as f32 * s.len() as f32 } else { 0.0 };
    }
    let grams = |v: &[char]| {
        let mut g: Vec<(char, char)> = v.windows(2).map(|w| (w[0], w[1])).collect();
        g.sort_unstable();
        g
    };
    let ga = grams(&a);
    let gb = grams(&b);
    // 多重集合の共通部分
    let (mut i, mut j, mut common) = (0, 0, 0usize);
    while i < ga.len() && j < gb.len() {
        match ga[i].cmp(&gb[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
        }
    }
    let dice = 2.0 * common as f32 / (ga.len() + gb.len()) as f32;
    let contain = common as f32 / ga.len().min(gb.len()) as f32;
    // 含まれる度合いは短すぎる方では当てにならないので、長さが 4 文字以上のときだけ使う
    if a.len().min(b.len()) >= 4 { dice.max(contain) } else { dice }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths() {
        assert_eq!(text_width("abc"), 3);
        assert_eq!(text_width("日本語"), 6);
        assert_eq!(text_width("ｶﾀｶﾅ"), 4); // 半角カナ
        assert_eq!(text_width("ＡＢ。"), 6);
    }

    #[test]
    fn japanese_breaks_after_punctuation() {
        // 16 全角 = 幅 32 の行
        let t = "今日は良い天気ですね。明日の講義では、微分方程式の基礎について説明します。";
        let lines = wrap_lines(t, 32);
        assert!(lines.iter().all(|l| text_width(l) <= 32), "{lines:?}");
        assert_eq!(lines[0], "今日は良い天気ですね。");
        assert_eq!(lines.concat(), t);
    }

    #[test]
    fn kinsoku_rules() {
        // 句点や小さい「っ」を行頭に置かない
        let t = "ああああああああああ。いいいいいいいいいいいいいっていう";
        for w in 8..30 {
            for l in wrap_lines(t, w) {
                let first = l.chars().next().unwrap();
                assert!(!NO_START.contains(first), "w={w} line={l}");
            }
        }
        let t = "これは「かぎかっこ」の例「です」ね";
        for w in 6..20 {
            for l in wrap_lines(t, w) {
                let last = l.chars().last().unwrap();
                assert!(!NO_END.contains(last), "w={w} line={l}");
            }
        }
    }

    #[test]
    fn latin_breaks_at_spaces() {
        let t = "The quick brown fox jumps over the lazy dog and keeps running far away";
        let lines = wrap_lines(t, 20);
        assert!(lines.iter().all(|l| l.len() <= 20), "{lines:?}");
        assert_eq!(lines.join(" "), t);
        // 切れ目のない長い語
        let lines = wrap_lines("supercalifragilisticexpialidocious word", 12);
        assert_eq!(lines[0], "supercalifragilisticexpialidocious");
    }

    #[test]
    fn mixed_and_pages() {
        let t = "SENCAはRTMPで受けた映像をHLSで配信し、同時に録画もします。字幕はWebVTTで出します。";
        let pages = split_pages(t, 32, 2);
        for p in &pages {
            assert!(p.lines().count() <= 2);
            assert!(p.lines().all(|l| text_width(l) <= 32), "{p}");
        }
        assert_eq!(pages.iter().map(|p| p.replace('\n', "")).collect::<String>(), t);
    }

    #[test]
    fn similarity_scores() {
        assert!(similarity("今日は良い天気ですね", "今日は良い天気ですね。") > 0.99);
        assert!(similarity("今日は良い天気", "今日は良い天気ですね") > 0.8); // 含まれる
        assert!(similarity("今日は良い天気ですね", "明日の講義について") < 0.2);
        assert!(similarity("Hello, World!", "hello world") > 0.99);
        assert_eq!(similarity("", ""), 1.0);
        assert_eq!(similarity("a", ""), 0.0);
    }

    #[test]
    fn spaces_normalized() {
        assert_eq!(normalize_spaces("  a \n\t b\u{0007}c  "), "a bc");
    }
}
