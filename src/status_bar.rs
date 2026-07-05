use crate::marker::MarkerKind;

/// Build the display string for context markers, handling nested checkbox compaction.
/// Returns a vector of (display_string, depth) tuples.
pub fn build_context_display(markers: &[MarkerKind]) -> Vec<(String, usize)> {
    let mut result = Vec::new();
    let mut depth = 0;
    let mut prev_was_checkbox = false;

    for (i, marker) in markers.iter().enumerate() {
        // Skip list marker if previous was checkbox (they're grouped together)
        if marker.is_list_item() && prev_was_checkbox {
            if i > 0 {
                depth += 1;
            }
            continue;
        }

        // Increment depth before block-level markers (except first)
        if i > 0 && marker.is_block_level() {
            depth += 1;
        }

        // Determine display string (nested checkboxes use compact form)
        let display_str = match marker {
            MarkerKind::Checkbox { checked: false } if prev_was_checkbox => " ]".to_string(),
            MarkerKind::Checkbox { checked: true } if prev_was_checkbox => "x]".to_string(),
            _ => marker.status_bar_str(),
        };

        result.push((display_str, depth));
        prev_was_checkbox = marker.is_checkbox();
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::marker::{OrderedMarker, UnorderedMarker};

    fn unordered_list(marker: UnorderedMarker) -> MarkerKind {
        MarkerKind::ListItem {
            ordered: false,
            unordered_marker: Some(marker),
            ordered_marker: None,
            number: None,
        }
    }

    fn ordered_list(number: u32, style: OrderedMarker) -> MarkerKind {
        MarkerKind::ListItem {
            ordered: true,
            unordered_marker: None,
            ordered_marker: Some(style),
            number: Some(number),
        }
    }

    #[test]
    fn status_bar_str_blockquote() {
        assert_eq!(MarkerKind::BlockQuote.status_bar_str(), ">");
    }

    #[test]
    fn status_bar_str_unordered_list_minus() {
        assert_eq!(unordered_list(UnorderedMarker::Minus).status_bar_str(), "-");
    }

    #[test]
    fn status_bar_str_unordered_list_star() {
        assert_eq!(unordered_list(UnorderedMarker::Star).status_bar_str(), "*");
    }

    #[test]
    fn status_bar_str_unordered_list_plus() {
        assert_eq!(unordered_list(UnorderedMarker::Plus).status_bar_str(), "+");
    }

    #[test]
    fn status_bar_str_ordered_list_dot() {
        assert_eq!(ordered_list(1, OrderedMarker::Dot).status_bar_str(), "1.");
        assert_eq!(ordered_list(42, OrderedMarker::Dot).status_bar_str(), "42.");
    }

    #[test]
    fn status_bar_str_ordered_list_parenthesis() {
        assert_eq!(
            ordered_list(1, OrderedMarker::Parenthesis).status_bar_str(),
            "1)"
        );
        assert_eq!(
            ordered_list(10, OrderedMarker::Parenthesis).status_bar_str(),
            "10)"
        );
    }

    #[test]
    fn status_bar_str_checkbox_unchecked() {
        assert_eq!(
            MarkerKind::Checkbox { checked: false }.status_bar_str(),
            "[ ]"
        );
    }

    #[test]
    fn status_bar_str_checkbox_checked() {
        assert_eq!(
            MarkerKind::Checkbox { checked: true }.status_bar_str(),
            "[x]"
        );
    }

    #[test]
    fn status_bar_str_code_block_no_language() {
        assert_eq!(
            MarkerKind::CodeBlockFence {
                language: None,
                is_opening: true
            }
            .status_bar_str(),
            "```"
        );
    }

    #[test]
    fn status_bar_str_code_block_with_language() {
        assert_eq!(
            MarkerKind::CodeBlockFence {
                language: Some("rust".to_string()),
                is_opening: true
            }
            .status_bar_str(),
            "```rust"
        );
    }

    #[test]
    fn display_simple_list() {
        let markers = vec![unordered_list(UnorderedMarker::Minus)];
        let display = build_context_display(&markers);
        assert_eq!(display, vec![("-".to_string(), 0)]);
    }

    #[test]
    fn display_nested_lists() {
        let markers = vec![
            unordered_list(UnorderedMarker::Minus),
            unordered_list(UnorderedMarker::Minus),
        ];
        let display = build_context_display(&markers);
        assert_eq!(display, vec![("-".to_string(), 0), ("-".to_string(), 1)]);
    }

    #[test]
    fn display_list_with_checkbox() {
        // - [x] displays as "- [x]" at same depth
        let markers = vec![
            unordered_list(UnorderedMarker::Minus),
            MarkerKind::Checkbox { checked: true },
        ];
        let display = build_context_display(&markers);
        assert_eq!(display, vec![("-".to_string(), 0), ("[x]".to_string(), 0)]);
    }

    #[test]
    fn display_nested_checkboxes_compact() {
        // - [x] - [ ] displays as "- [x] ]" (nested checkbox compacted)
        let markers = vec![
            unordered_list(UnorderedMarker::Minus),
            MarkerKind::Checkbox { checked: true },
            unordered_list(UnorderedMarker::Minus),
            MarkerKind::Checkbox { checked: false },
        ];
        let display = build_context_display(&markers);
        // After [x], the next - is skipped, depth increments, then [ ] shows as " ]"
        assert_eq!(
            display,
            vec![
                ("-".to_string(), 0),
                ("[x]".to_string(), 0),
                (" ]".to_string(), 1)
            ]
        );
    }

    #[test]
    fn display_nested_checked_checkbox_compact() {
        // - [ ] - [x] displays as "- [ ]x]"
        let markers = vec![
            unordered_list(UnorderedMarker::Minus),
            MarkerKind::Checkbox { checked: false },
            unordered_list(UnorderedMarker::Minus),
            MarkerKind::Checkbox { checked: true },
        ];
        let display = build_context_display(&markers);
        assert_eq!(
            display,
            vec![
                ("-".to_string(), 0),
                ("[ ]".to_string(), 0),
                ("x]".to_string(), 1)
            ]
        );
    }

    #[test]
    fn display_blockquote_list() {
        let markers = vec![
            MarkerKind::BlockQuote,
            unordered_list(UnorderedMarker::Minus),
        ];
        let display = build_context_display(&markers);
        assert_eq!(display, vec![(">".to_string(), 0), ("-".to_string(), 1)]);
    }

    #[test]
    fn display_ordered_list() {
        let markers = vec![ordered_list(3, OrderedMarker::Dot)];
        let display = build_context_display(&markers);
        assert_eq!(display, vec![("3.".to_string(), 0)]);
    }

    #[test]
    fn display_code_block() {
        let markers = vec![MarkerKind::CodeBlockFence {
            language: Some("python".to_string()),
            is_opening: true,
        }];
        let display = build_context_display(&markers);
        assert_eq!(display, vec![("```python".to_string(), 0)]);
    }

    #[test]
    fn display_deeply_nested() {
        // > - [x] - [ ] - [x]
        let markers = vec![
            MarkerKind::BlockQuote,
            unordered_list(UnorderedMarker::Minus),
            MarkerKind::Checkbox { checked: true },
            unordered_list(UnorderedMarker::Minus),
            MarkerKind::Checkbox { checked: false },
            unordered_list(UnorderedMarker::Minus),
            MarkerKind::Checkbox { checked: true },
        ];
        let display = build_context_display(&markers);
        // > at depth 0, - [x] at depth 1, ] at depth 2, x] at depth 3
        assert_eq!(
            display,
            vec![
                (">".to_string(), 0),
                ("-".to_string(), 1),
                ("[x]".to_string(), 1),
                (" ]".to_string(), 2),
                ("x]".to_string(), 3),
            ]
        );
    }

    #[test]
    fn depth_cycles_after_six() {
        // 7 nested lists should cycle back to depth 0 for color
        let markers = vec![
            unordered_list(UnorderedMarker::Minus),
            unordered_list(UnorderedMarker::Minus),
            unordered_list(UnorderedMarker::Minus),
            unordered_list(UnorderedMarker::Minus),
            unordered_list(UnorderedMarker::Minus),
            unordered_list(UnorderedMarker::Minus),
            unordered_list(UnorderedMarker::Minus),
        ];
        let display = build_context_display(&markers);
        assert_eq!(display.len(), 7);
        // Depths: 0, 1, 2, 3, 4, 5, 6
        assert_eq!(display[6].1, 6);
        // Color cycling happens in status_bar() with depth % 6
    }
}
