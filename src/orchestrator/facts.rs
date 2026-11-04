//! Проверка, что числа в ответе модели взяты из данных, а не придуманы.
//!
//! Эвристика: из ответа извлекаются «значимые» числа (с дробной частью или от 1000),
//! и каждое должно встречаться в известных тексте: результатах функций, вопросе, истории.
//! Ответ не блокируется, но в ответ API и в журнал добавляется предупреждение.
//! Вычисленные моделью значения (суммы, разности) тоже попадут в предупреждение: так и задумано.

use std::collections::HashSet;

/// Число в сотых долях, без знака.
pub type Hundredths = u128;

const SIGNIFICANT_MIN: Hundredths = 1000 * 100;

fn is_group_space(c: char) -> bool {
    matches!(c, ' ' | '\u{a0}' | '\u{202f}' | '\u{2009}')
}

/// Извлекает числа из текста. Понимает «1 250 000,50», «1250000.50», «12 500».
/// Даты вида 01.11.2026 и номера вида 1.2.3 пропускаются.
pub fn extract_numbers(text: &str) -> Vec<(Hundredths, bool)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() || (i > 0 && chars[i - 1].is_alphanumeric()) {
            i += 1;
            continue;
        }
        let start_run = i;
        let mut int_digits = String::new();
        while i < chars.len() && chars[i].is_ascii_digit() {
            int_digits.push(chars[i]);
            i += 1;
        }
        // Группы разрядов через пробел: только если первая группа не длиннее трёх цифр.
        if i - start_run <= 3 {
            while i < chars.len()
                && is_group_space(chars[i])
                && chars
                    .get(i + 1..i + 4)
                    .is_some_and(|g| g.iter().all(char::is_ascii_digit))
                && !chars.get(i + 4).is_some_and(char::is_ascii_digit)
            {
                int_digits.extend(&chars[i + 1..i + 4]);
                i += 4;
            }
        }
        let mut frac = String::new();
        let mut skip = false;
        if i + 1 < chars.len() && matches!(chars[i], '.' | ',') && chars[i + 1].is_ascii_digit() {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            let run = j - (i + 1);
            let followed_by_more = j + 1 < chars.len()
                && matches!(chars[j], '.' | ',')
                && chars[j + 1].is_ascii_digit();
            if run <= 2 && !followed_by_more {
                frac = chars[i + 1..j].iter().collect();
                i = j;
            } else if chars[i] == '.' {
                // Дата или номер версии: пропускаем всю конструкцию.
                skip = true;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
            }
        }
        if skip {
            continue;
        }
        let int: Option<Hundredths> = int_digits.parse().ok();
        let frac_val: Hundredths = match frac.len() {
            1 => frac.parse::<Hundredths>().unwrap_or(0) * 10,
            2 => frac.parse::<Hundredths>().unwrap_or(0),
            _ => 0,
        };
        if let Some(v) = int
            .and_then(|v| v.checked_mul(100))
            .and_then(|v| v.checked_add(frac_val))
        {
            out.push((v, !frac.is_empty()));
        }
    }
    out
}

/// Множество чисел, встречающихся в известных источниках.
#[derive(Debug, Default)]
pub struct KnownNumbers(HashSet<Hundredths>);

impl KnownNumbers {
    pub fn add_text(&mut self, text: &str) {
        self.0
            .extend(extract_numbers(text).into_iter().map(|(v, _)| v));
    }

    pub fn contains(&self, v: Hundredths) -> bool {
        self.0.contains(&v)
    }
}

/// Значимые числа ответа, которых нет среди известных.
pub fn unverified_numbers(answer: &str, known: &KnownNumbers) -> Vec<String> {
    let mut seen = HashSet::new();
    extract_numbers(answer)
        .into_iter()
        .filter(|(v, has_frac)| *has_frac || *v >= SIGNIFICANT_MIN)
        .filter(|(v, _)| !known.contains(*v))
        .filter(|(v, _)| seen.insert(*v))
        .map(|(v, _)| format!("{}.{:02}", v / 100, v % 100))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nums(text: &str) -> Vec<Hundredths> {
        extract_numbers(text).into_iter().map(|(v, _)| v).collect()
    }

    #[test]
    fn parses_russian_formats() {
        assert_eq!(nums("долг 1 250 000,50 руб."), vec![125_000_050]);
        assert_eq!(nums("1250000.50"), vec![125_000_050]);
        assert_eq!(nums("12\u{a0}500 ₽"), vec![1_250_000]);
        assert_eq!(nums("на 01.11.2026 сумма 300"), vec![30_000]);
        assert_eq!(nums("ИНН 7701234567"), vec![770_123_456_700]);
        assert_eq!(nums("cp-001 и СЧ-0007"), vec![100, 700]);
        assert_eq!(nums("в 2026 году 15 000"), vec![202_600, 1_500_000]);
    }

    #[test]
    fn flags_only_unknown_significant_numbers() {
        let mut known = KnownNumbers::default();
        known.add_text(r#"{"balance":"125000.50","date":"2026-11-01"}"#);
        assert!(
            unverified_numbers("Сальдо 125 000,50 руб. на 2026-11-01, счетов 2", &known).is_empty()
        );
        assert_eq!(
            unverified_numbers("Итого 130 000,00 руб.", &known),
            vec!["130000.00".to_string()]
        );
    }
}
