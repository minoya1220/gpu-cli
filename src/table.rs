//! Shared table alignment, labels and count nouns.
pub fn plural(count: usize, word: &str) -> String {
    format!("{count} {}", noun(count, word))
}
pub(crate) fn noun(count: usize, word: &str) -> String {
    format!("{word}{}", if count == 1 { "" } else { "s" })
}

#[derive(Clone, Copy)]
pub(crate) enum Column {
    Left,
    Right,
    Plain,
}

/// Measured widths and double-space gutters. Plain cells keep their formatting.
pub(crate) fn aligned_rows<const N: usize>(
    rows: &[[String; N]],
    columns: [Column; N],
) -> Vec<String> {
    let widths: [usize; N] =
        std::array::from_fn(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0));
    rows.iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(i, cell)| match columns[i] {
                    Column::Left => format!("{cell:<w$}", w = widths[i]),
                    Column::Right => format!("{cell:>w$}", w = widths[i]),
                    Column::Plain => cell.clone(),
                })
                .collect::<Vec<_>>()
                .join("  ")
        })
        .collect()
}

/// Hardware discovery uses one four-column grid across providers.
pub(crate) type HardwareColumns = [String; 4];

pub(crate) fn hardware_labels(rows: &[HardwareColumns]) -> Vec<String> {
    aligned_rows(
        rows,
        [Column::Left, Column::Right, Column::Left, Column::Plain],
    )
    .into_iter()
    .map(|label| label.trim_end().to_string())
    .collect()
}
