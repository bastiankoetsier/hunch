//! Human-readable output.
//!
//! Every function here is pure: it takes answer data and returns the text to
//! print. Nothing writes to stdout directly, which keeps rendering trivially
//! unit-testable (compare strings) and leaves the decision of *where* output
//! goes (stdout, stderr, a test buffer) to the caller.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::wire::{Answer, Response};

/// Width of every probability bar, in characters. Fixed so that bars line up
/// and output is stable regardless of the values.
pub const BAR_WIDTH: usize = 20;

/// Renders any answer, dispatching on its type.
pub fn answer(answer: &Answer) -> String {
    match answer {
        Answer::Noul { noul } => self::noul(*noul),
        Answer::Choice {
            choice,
            probabilities,
            confidence,
        } => self::choice(choice, probabilities, *confidence),
        Answer::Score {
            score,
            legend,
            probabilities,
            confidence,
        } => self::score(*score, legend, probabilities, *confidence),
        Answer::Refusal => "refused: the model declined to answer\n".to_string(),
    }
}

/// `0.87  █████████████████░░░  likely yes`
pub fn noul(probability: f64) -> String {
    let hint = if probability >= 0.8 {
        "likely yes"
    } else if probability <= 0.2 {
        "likely no"
    } else {
        "uncertain"
    };
    format!("{probability:.2}  {}  {hint}\n", bar(probability))
}

/// The winner and its confidence, then every option, most likely first:
///
/// ```text
/// technical  (confidence 0.78)
///
///   technical  █████████████████░░░   85.0%
///   billing    ███░░░░░░░░░░░░░░░░░   15.0%
/// ```
///
/// Not every model reports a confidence or a distribution; whichever is
/// missing is left out.
pub fn choice(
    choice: &str,
    probabilities: &BTreeMap<String, f64>,
    confidence: Option<f64>,
) -> String {
    let mut rows: Vec<(&str, f64)> = probabilities
        .iter()
        .map(|(name, p)| (name.as_str(), *p))
        .collect();
    // `f64` is not `Ord` (NaN breaks total ordering), so `sort_by_key` is out;
    // `total_cmp` defines an order for every float. `sort_by` is stable, so
    // ties keep the alphabetical order the `BTreeMap` iterated in.
    rows.sort_by(|a, b| b.1.total_cmp(&a.1));

    let mut out = format!("{choice}{}\n", confidence_suffix(confidence));
    let width = label_width(rows.iter().map(|(name, _)| *name));
    out += &table(rows.into_iter().map(|(name, p)| row(name, width, p)));
    out
}

/// The score on its scale with the nearest level, then every level in
/// order:
///
/// ```text
/// 1.05 on a 0–2 scale → Frustrated  (confidence 0.92)
///
///   0  Calm        ░░░░░░░░░░░░░░░░░░░░    0.0%
///   1  Frustrated  ███████████████████░   95.0%
///   2  Very angry  █░░░░░░░░░░░░░░░░░░░    5.0%
/// ```
pub fn score(
    score: f64,
    legend: &BTreeMap<String, String>,
    probabilities: &BTreeMap<String, f64>,
    confidence: Option<f64>,
) -> String {
    let levels = sorted_levels(legend);

    let mut out = format!("{score:.2}");
    if let (Some(first), Some(last)) = (levels.first(), levels.last()) {
        out += &format!(" on a {}–{} scale", first.key, last.key);
    }
    if let Some(nearest) = nearest_level(&levels, score) {
        out += &format!(" → {}", nearest.description);
    }
    out += &format!("{}\n", confidence_suffix(confidence));

    let labels: Vec<String> = levels
        .iter()
        .map(|level| format!("{}  {}", level.key, level.description))
        .collect();
    let width = label_width(labels.iter().map(String::as_str));
    out += &table(levels.iter().zip(&labels).map(|(level, label)| {
        let p = probabilities.get(level.key).copied().unwrap_or(0.0);
        row(label, width, p)
    }));
    out
}

/// One line for `--verbose`, meant for stderr:
/// `model jev-1.13.0, 392 input + 65 output tokens`.
pub fn usage(response: &Response) -> String {
    format!(
        "model {}, {} input + {} output tokens\n",
        response.model, response.usage.input_tokens, response.usage.output_tokens
    )
}

/// A fixed-width bar: `fraction` of [`BAR_WIDTH`] cells filled, rounded to
/// the nearest cell. Out-of-range input is clamped rather than trusted.
pub fn bar(fraction: f64) -> String {
    let filled = (fraction.clamp(0.0, 1.0) * BAR_WIDTH as f64).round() as usize;
    "█".repeat(filled) + &"░".repeat(BAR_WIDTH - filled)
}

/// `  (confidence 0.78)`, or nothing when the model reported none.
fn confidence_suffix(confidence: Option<f64>) -> String {
    confidence
        .map(|confidence| format!("  (confidence {confidence:.2})"))
        .unwrap_or_default()
}

/// The rows below a headline, set off by a blank line; nothing at all when
/// there are no rows, so the output never ends in a dangling blank line.
fn table(rows: impl Iterator<Item = String>) -> String {
    let rows: String = rows.collect();
    if rows.is_empty() {
        rows
    } else {
        format!("\n{rows}")
    }
}

/// `  label    ███░░…   15.0%`, with the label padded to `width`.
fn row(label: &str, width: usize, p: f64) -> String {
    // `{:<width$}` pads by `char` count, which suits plain labels; the
    // percentage is right-aligned in 5 columns so "5.0" and "100.0" align.
    format!("  {label:<width$}  {}  {:>5.1}%\n", bar(p), p * 100.0)
}

/// Widest label in characters (not bytes, so "é" counts as one).
fn label_width<'a>(labels: impl Iterator<Item = &'a str>) -> usize {
    labels.map(|label| label.chars().count()).max().unwrap_or(0)
}

/// A legend entry, borrowed from the response.
#[derive(Debug, PartialEq)]
struct Level<'a> {
    key: &'a str,
    description: &'a str,
}

/// Legend entries ordered by their numeric key.
///
/// The legend is a `BTreeMap<String, _>`, which iterates in *string* order:
/// "10" sorts before "9". Score levels are numbers, so we sort numerically,
/// and put any non-numeric keys (not expected from Jev) last, in string
/// order, instead of failing.
fn sorted_levels(legend: &BTreeMap<String, String>) -> Vec<Level<'_>> {
    let mut levels: Vec<Level> = legend
        .iter()
        .map(|(key, description)| Level { key, description })
        .collect();
    levels.sort_by(|a, b| match (a.key.parse::<f64>(), b.key.parse::<f64>()) {
        (Ok(x), Ok(y)) => x.total_cmp(&y),
        (Ok(_), Err(_)) => Ordering::Less,
        (Err(_), Ok(_)) => Ordering::Greater,
        (Err(_), Err(_)) => a.key.cmp(b.key),
    });
    levels
}

/// The level whose numeric key is closest to `score`.
fn nearest_level<'a, 'b>(levels: &'b [Level<'a>], score: f64) -> Option<&'b Level<'a>> {
    levels
        .iter()
        .filter_map(|level| Some((level, level.key.parse::<f64>().ok()?)))
        .min_by(|(_, x), (_, y)| (x - score).abs().total_cmp(&(y - score).abs()))
        .map(|(level, _)| level)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Usage;

    /// Builds a `BTreeMap<String, V>` from borrowed pairs. The `Into<V>`
    /// bound lets the same helper turn `&str` values into `String`s for a
    /// legend and pass `f64`s through unchanged for probabilities.
    fn map<T: Copy + Into<V>, V>(pairs: &[(&str, T)]) -> BTreeMap<String, V> {
        pairs
            .iter()
            .map(|&(key, value)| (key.to_string(), value.into()))
            .collect()
    }

    #[test]
    fn bar_has_fixed_width() {
        assert_eq!(bar(0.0), "░".repeat(20));
        assert_eq!(bar(1.0), "█".repeat(20));
        assert_eq!(bar(0.5), "█".repeat(10) + &"░".repeat(10));
        // Clamped, never wider or narrower.
        assert_eq!(bar(1.7).chars().count(), 20);
        assert_eq!(bar(-0.3).chars().count(), 20);
    }

    #[test]
    fn noul_shows_probability_bar_and_hint() {
        assert_eq!(noul(0.87), "0.87  █████████████████░░░  likely yes\n");
        assert!(noul(0.5).ends_with("  uncertain\n"));
        assert!(noul(0.2).ends_with("  likely no\n"));
        assert!(noul(0.8).ends_with("  likely yes\n"));
    }

    #[test]
    fn choice_lists_options_most_likely_first() {
        let probabilities: BTreeMap<String, f64> =
            map(&[("billing", 0.15), ("sales", 0.0), ("technical", 0.85)]);
        let expected = [
            "technical  (confidence 0.78)",
            "",
            "  technical  █████████████████░░░   85.0%",
            "  billing    ███░░░░░░░░░░░░░░░░░   15.0%",
            "  sales      ░░░░░░░░░░░░░░░░░░░░    0.0%",
            "",
        ]
        .join("\n");
        assert_eq!(choice("technical", &probabilities, Some(0.78)), expected);
    }

    #[test]
    fn score_shows_scale_nearest_level_and_levels_in_order() {
        let legend: BTreeMap<String, String> =
            map(&[("0", "Calm"), ("1", "Frustrated"), ("2", "Very angry")]);
        let probabilities: BTreeMap<String, f64> = map(&[("0", 0.0), ("1", 0.95), ("2", 0.05)]);
        let expected = [
            "1.05 on a 0–2 scale → Frustrated  (confidence 0.92)",
            "",
            "  0  Calm        ░░░░░░░░░░░░░░░░░░░░    0.0%",
            "  1  Frustrated  ███████████████████░   95.0%",
            "  2  Very angry  █░░░░░░░░░░░░░░░░░░░    5.0%",
            "",
        ]
        .join("\n");
        assert_eq!(score(1.05, &legend, &probabilities, Some(0.92)), expected);
    }

    #[test]
    fn score_sorts_levels_numerically_not_lexically() {
        let legend: BTreeMap<String, String> =
            (0..=10).map(|n| (n.to_string(), format!("L{n}"))).collect();
        // String order would be 0, 1, 10, 2, ...; we want 0, 1, 2, ..., 10.
        let keys: Vec<&str> = sorted_levels(&legend).iter().map(|l| l.key).collect();
        assert_eq!(
            keys,
            ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10"]
        );

        let out = score(9.7, &legend, &BTreeMap::new(), Some(0.5));
        assert!(out.starts_with("9.70 on a 0–10 scale → L10"), "{out}");
        let last_row = out.lines().last().unwrap();
        assert!(last_row.trim_start().starts_with("10  L10"), "{last_row}");
    }

    #[test]
    fn sparse_answers_render_just_the_headline() {
        assert_eq!(choice("billing", &BTreeMap::new(), None), "billing\n");
        assert_eq!(
            score(1.1, &BTreeMap::new(), &BTreeMap::new(), Some(0.55)),
            "1.10  (confidence 0.55)\n"
        );
    }

    #[test]
    fn usage_line_names_model_and_tokens() {
        let response = Response {
            model: "jev-1.13.0".into(),
            answers: BTreeMap::new(),
            usage: Usage {
                input_tokens: 392,
                output_tokens: 65,
            },
        };
        assert_eq!(
            usage(&response),
            "model jev-1.13.0, 392 input + 65 output tokens\n"
        );
    }
}
