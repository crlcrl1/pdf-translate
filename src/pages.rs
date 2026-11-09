use std::str::FromStr;

use anyhow::{Result, bail};

/// 1-based page selection such as `1-3,5,8-`.
#[derive(Debug, Clone, Default)]
pub struct PageSelection {
    ranges: Vec<(usize, Option<usize>)>,
}

impl PageSelection {
    pub fn all() -> Self {
        Self {
            ranges: vec![(1, None)],
        }
    }

    pub fn contains(&self, page_1based: usize) -> bool {
        self.ranges
            .iter()
            .any(|(start, end)| page_1based >= *start && end.is_none_or(|end| page_1based <= end))
    }
}

impl FromStr for PageSelection {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.is_empty() || s == "all" {
            return Ok(Self::all());
        }

        let mut ranges = Vec::new();
        for part in s.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if let Some((start, end)) = part.split_once('-') {
                let start = if start.trim().is_empty() {
                    1
                } else {
                    start.trim().parse::<usize>()?
                };
                let end = if end.trim().is_empty() {
                    None
                } else {
                    Some(end.trim().parse::<usize>()?)
                };
                if start == 0 || end.is_some_and(|e| e < start) {
                    bail!("invalid page range: {part}");
                }
                ranges.push((start, end));
            } else {
                let page = part.parse::<usize>()?;
                if page == 0 {
                    bail!("page numbers are 1-based");
                }
                ranges.push((page, Some(page)));
            }
        }

        if ranges.is_empty() {
            bail!("page selection is empty");
        }
        Ok(Self { ranges })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ranges() {
        let selection: PageSelection = "1-3,5,8-".parse().unwrap();
        assert!(selection.contains(1));
        assert!(selection.contains(3));
        assert!(!selection.contains(4));
        assert!(selection.contains(5));
        assert!(selection.contains(100));
    }

    #[test]
    fn rejects_zero_and_reverse_ranges() {
        assert!("0-2".parse::<PageSelection>().is_err());
        assert!("5-3".parse::<PageSelection>().is_err());
    }

    #[test]
    fn all_pages() {
        let selection: PageSelection = "all".parse().unwrap();
        assert!(selection.contains(1));
        assert!(selection.contains(999));
    }
}
