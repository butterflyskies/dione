//! Discord author binding and tool-wire projection for sentex locators.

mod error;
use error::{HandleError, HandleErrorKind, SentexField};
pub(crate) use error::{SentexError, SentexErrorKind};
pub(crate) mod locators;
pub(crate) use locators::*;
use serde_json::{Value, json};
use serenity::model::id::UserId;

/// A role-bearing sentex reference offered by the Discord message author.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OfferedSentex {
    pub(crate) locator: SentexLocator,
    pub(crate) author_id: UserId,
}

/// Parses the role-separated `claim_handles` and `citation_handles` arguments.
/// The removed `evidence_keys` name is rejected rather than silently ignored.
pub(crate) fn parse_tool_sentex_handles(args: &Value) -> Result<SentexHandles, SentexError> {
    if args.get("evidence_keys").is_some() {
        return Err(SentexError {
            kind: SentexErrorKind::RemovedLegacyField,
        });
    }
    let claims = handle_array(args, SentexField::Claims)?;
    let citations = handle_array(args, SentexField::Citations)?;
    let total = claims
        .len()
        .checked_add(citations.len())
        .ok_or(SentexError {
            kind: SentexErrorKind::CountOverflow,
        })?;
    if total > MAX_SENTEX_REFS {
        return Err(SentexError {
            kind: SentexErrorKind::TooManyReferences,
        });
    }
    let handles = SentexHandles {
        claims: parse_handle_array(claims, SentexField::Claims)?,
        citations: parse_handle_array(citations, SentexField::Citations)?,
    };
    validate_marker_bytes(&handles)?;
    Ok(handles)
}
fn handle_array(args: &Value, field: SentexField) -> Result<&[Value], SentexError> {
    let Some(value) = args.get(field.as_str()) else {
        return Ok(&[]);
    };
    value.as_array().map(Vec::as_slice).ok_or(SentexError {
        kind: SentexErrorKind::ExpectedArray(field),
    })
}
fn parse_handle_array(
    values: &[Value],
    field: SentexField,
) -> Result<Vec<VaeliiSentexHandle>, SentexError> {
    values
        .iter()
        .map(|value| {
            let text = value.as_str().ok_or(HandleError {
                kind: HandleErrorKind::Noncanonical,
            });
            text.and_then(VaeliiSentexHandle::parse_decimal)
                .map_err(|cause| SentexError {
                    kind: SentexErrorKind::InvalidHandle { field, cause },
                })
        })
        .collect()
}

/// Binds author-free parsed locators to Discord's system-derived author.
pub(crate) fn parse_offered_sentexes(content: &str, author_id: UserId) -> Vec<OfferedSentex> {
    parse_sentex_locators(content)
        .into_iter()
        .map(|locator| OfferedSentex { locator, author_id })
        .collect()
}

fn offered_sentex_json(offerings: &[OfferedSentex], role: SentexRole) -> Vec<Value> {
    offerings
        .iter()
        .filter(|sentex| sentex.locator.role() == role)
        .map(|sentex| {
            json!({
                "locator": sentex.locator.as_str(),
                "author_id": sentex.author_id.get().to_string(),
            })
        })
        .collect()
}

/// Projects already parsed offerings without changing empty response shapes.
pub(crate) fn project_offered_sentexes(target: &mut Value, offerings: &[OfferedSentex]) {
    let claims = offered_sentex_json(offerings, SentexRole::Claim);
    let citations = offered_sentex_json(offerings, SentexRole::Citation);
    if !claims.is_empty() {
        target["claim_locators"] = json!(claims);
    }
    if !citations.is_empty() {
        target["citation_locators"] = json!(citations);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::num::NonZeroU64;

    #[test]
    fn legacy_terminal_markers_parse_as_citations_with_discord_author() {
        let content = "claim [🔍=v1:AAAAAAAAAAw] [🔍=v1:AAAAAAAAACI]";
        let sentexes = parse_offered_sentexes(content, UserId::new(99));
        assert_eq!(sentexes.len(), 2);
        assert_eq!(sentexes[0].locator.as_str(), "v1:AAAAAAAAAAw");
        assert_eq!(sentexes[1].locator.as_str(), "v1:AAAAAAAAACI");
        assert!(
            sentexes.iter().all(|item| item.author_id == UserId::new(99)
                && item.locator.role() == SentexRole::Citation)
        );
    }

    #[test]
    fn v2_terminal_markers_preserve_roles_and_original_order() {
        let content = concat!(
            "claim ",
            "[🔍=v2:citation:AAAAAAAAACI] ",
            "[🔍=v2:claim:AAAAAAAAAAw]"
        );
        let sentexes = parse_offered_sentexes(content, UserId::new(99));
        assert_eq!(sentexes.len(), 2);
        assert_eq!(sentexes[0].locator.role(), SentexRole::Citation);
        assert_eq!(sentexes[0].locator.as_str(), "v2:citation:AAAAAAAAACI");
        assert_eq!(sentexes[1].locator.role(), SentexRole::Claim);
        assert_eq!(sentexes[1].locator.as_str(), "v2:claim:AAAAAAAAAAw");
    }

    #[test]
    fn malformed_quoted_and_mid_message_markers_are_prose() {
        for content in [
            "claim [🔍=v1:AAAAAAAAAAA]",
            "claim [🔍=v1:short]",
            "claim [🔍=v1:AAAAAAAAAAw=]",
            "claim [🔍=v2:AAAAAAAAAAw]",
            "claim [🔍=v1:AAAAAAAAAAw",
            "claim[🔍=v1:AAAAAAAAAAw]",
            "claim [🔍=v1:AAAAAAAAAAw] afterward",
            "example `[🔍=v1:AAAAAAAAAAw]`",
            "> quoted claim [🔍=v1:AAAAAAAAAAw]",
            ">>> quoted claim\nstill quoted [🔍=v1:AAAAAAAAAAw]",
            "```\ncode example [🔍=v1:AAAAAAAAAAw]",
            "```rust\nlet marker = \"example\"; [🔍=v1:AAAAAAAAAAw]",
        ] {
            assert_eq!(parse_offered_sentexes(content, UserId::new(99)), vec![]);
        }
    }

    #[test]
    fn unmatched_inline_code_marker_is_prose() {
        for content in [
            "example `claim [🔍=v1:AAAAAAAAAAw]",
            "example ``claim [🔍=v1:AAAAAAAAAAw]",
            "example `code\nclaim [🔍=v1:AAAAAAAAAAw]",
            "example ``code\nclaim [🔍=v1:AAAAAAAAAAw]",
            "example ```claim [🔍=v1:AAAAAAAAAAw]",
            "example ````claim [🔍=v1:AAAAAAAAAAw]",
        ] {
            assert!(parse_sentex_locators(content).is_empty());
        }
    }

    #[test]
    fn quote_syntax_inside_a_closed_fence_does_not_poison_later_claims() {
        for content in [
            "```\n>>> code example\n```\nclaim [🔍=v1:AAAAAAAAAAw]",
            "```\n    ```\nstill code\n```\nclaim [🔍=v1:AAAAAAAAAAw]",
            "`inline\n>>> code example\nclosed` claim [🔍=v1:AAAAAAAAAAw]",
        ] {
            assert_eq!(parse_sentex_locators(content)[0].as_str(), "v1:AAAAAAAAAAw");
        }
    }

    #[test]
    fn four_space_indented_backticks_do_not_close_an_open_fence() {
        let content = "```\n    ```\nstill code [🔍=v1:AAAAAAAAAAw]";
        assert!(parse_sentex_locators(content).is_empty());
    }

    #[test]
    fn four_space_indented_backticks_do_not_poison_a_later_claim() {
        // Indented code block after a blank line — backticks are literal.
        let content = "    ```\nclaim [🔍=v1:AAAAAAAAAAw]";
        let locators = parse_sentex_locators(content);
        assert_eq!(locators.len(), 1);
        assert_eq!(locators[0].as_str(), "v1:AAAAAAAAAAw");
    }

    #[test]
    fn paragraph_continuation_indented_backtick_poisons_later_claim() {
        // No blank line between paragraph and indented line — the indented
        // line is a paragraph continuation under CommonMark, NOT indented code.
        // The backtick opens an inline code span that crosses into the next
        // line, poisoning the evidence marker.
        for content in [
            // Single backtick on indented paragraph continuation
            "paragraph text\n    `unclosed span\nclaim [🔍=v1:AAAAAAAAAAw]",
            // Double backtick on indented paragraph continuation
            "paragraph text\n    ``unclosed span\nclaim [🔍=v1:AAAAAAAAAAw]",
        ] {
            assert!(
                parse_sentex_locators(content).is_empty(),
                "paragraph-continuation indented backtick must poison: {content:?}"
            );
        }
    }

    #[test]
    fn genuine_indented_code_backtick_does_not_poison_claim() {
        // Blank line before indented line — genuine indented code block.
        // Backtick is literal code content, should NOT open an inline span.
        let content = "paragraph text\n\n    `indented code\nclaim [🔍=v1:AAAAAAAAAAw]";
        let locators = parse_sentex_locators(content);
        assert_eq!(locators.len(), 1);
        assert_eq!(locators[0].as_str(), "v1:AAAAAAAAAAw");
    }

    #[test]
    fn marker_on_indented_code_line_is_rejected() {
        // The marker itself is on a 4+-space indented line at document start
        // (indented code block). Must be rejected.
        for content in [
            "    claim [🔍=v1:AAAAAAAAAAw]",
            "\t claim [🔍=v1:AAAAAAAAAAw]",
            "paragraph\n\n    claim [🔍=v1:AAAAAAAAAAw]",
        ] {
            assert!(
                parse_sentex_locators(content).is_empty(),
                "marker on indented code line must be rejected: {content:?}"
            );
        }
    }

    #[test]
    fn consecutive_indented_lines_remain_in_code_block() {
        // Multiple contiguous 4+-space lines after a blank are all indented
        // code — backticks on the second line must not open a span.
        for content in [
            "paragraph\n\n    line 1\n    `line 2\nclaim [🔍=v1:AAAAAAAAAAw]",
            "\n    line 1\n    line 2\n    line 3\nclaim [🔍=v1:AAAAAAAAAAw]",
        ] {
            let locators = parse_sentex_locators(content);
            assert_eq!(
                locators.len(),
                1,
                "claim after exiting contiguous indented code must parse: {content:?}"
            );
        }
    }

    #[test]
    fn indented_code_after_heading_is_rejected() {
        // After a heading (not a paragraph), indented code CAN start
        // without a blank line. A marker there is in code.
        let content = "# Heading\n    claim [🔍=v1:AAAAAAAAAAw]";
        assert!(
            parse_sentex_locators(content).is_empty(),
            "marker in indented code after heading must be rejected"
        );
    }

    #[test]
    fn indented_code_after_fence_close_is_rejected() {
        // After a fence closes, a 4+-space line is indented code (not a
        // paragraph continuation).
        let content = "```\ncode\n```\n    claim [🔍=v1:AAAAAAAAAAw]";
        assert!(
            parse_sentex_locators(content).is_empty(),
            "marker in indented code after fence close must be rejected"
        );
    }

    #[test]
    fn blank_line_inside_indented_block_exits_code() {
        // A blank line inside indented content ends the code block.
        // The next indented line after a paragraph is a continuation.
        let content =
            "paragraph\n\n    code line\n\nparagraph 2\n    `backtick\nclaim [🔍=v1:AAAAAAAAAAw]";
        assert!(
            parse_sentex_locators(content).is_empty(),
            "backtick on paragraph continuation after code block must poison"
        );
    }

    #[test]
    fn unindented_line_exits_code_block() {
        // After an unindented line, we're in a paragraph — indented lines
        // are continuations.
        let content = "\n    code\nparagraph\n    `backtick\nclaim [🔍=v1:AAAAAAAAAAw]";
        assert!(
            parse_sentex_locators(content).is_empty(),
            "backtick on paragraph continuation after exiting code must poison"
        );
    }

    #[test]
    fn closed_quote_and_inline_code_do_not_poison_later_claims() {
        for content in [
            "> quoted example\nclaim [🔍=v1:AAAAAAAAAAw]",
            "`inline example` claim [🔍=v1:AAAAAAAAAAw]",
            "``inline example`` claim [🔍=v1:AAAAAAAAAAw]",
            "`inline\nexample` claim [🔍=v1:AAAAAAAAAAw]",
            "example ```inline``` claim [🔍=v1:AAAAAAAAAAw]",
            "example ````inline\nexample```` claim [🔍=v1:AAAAAAAAAAw]",
        ] {
            assert_eq!(parse_sentex_locators(content)[0].as_str(), "v1:AAAAAAAAAAw");
        }
    }

    #[test]
    fn locator_requires_exact_nonzero_eight_byte_canonical_payload() {
        assert!(VaeliiSentexHandle::from_locator_token("AAAAAAAAAAA").is_none());
        assert!(VaeliiSentexHandle::from_locator_token("AAAAAAAAAAw=").is_none());
        assert!(VaeliiSentexHandle::from_locator_token("AAAAAAAAAA").is_none());
        assert!(VaeliiSentexHandle::from_locator_token("AAAAAAAAAAAA").is_none());
        assert_eq!(
            VaeliiSentexHandle::from_locator_token("__________8"),
            Some(VaeliiSentexHandle(NonZeroU64::new(u64::MAX).unwrap()))
        );
    }

    #[test]
    fn tool_input_is_role_separated_canonical_and_combined_bounded() {
        assert!(parse_tool_sentex_handles(&json!({})).unwrap().is_empty());
        assert!(parse_tool_sentex_handles(&json!({ "evidence_keys": ["1"] })).is_err());
        for field in ["claim_handles", "citation_handles"] {
            for invalid in [
                json!([1]),
                json!([9_007_199_254_740_993u64]),
                json!(["0"]),
                json!(["01"]),
                json!(["-1"]),
                json!(["abc"]),
                json!(["18446744073709551616"]),
                json!("1"),
            ] {
                assert!(
                    parse_tool_sentex_handles(&json!({ field: invalid })).is_err(),
                    "{field} accepted a noncanonical handle"
                );
            }
        }
        assert!(
            parse_tool_sentex_handles(&json!({
                "claim_handles": ["1", "2"],
                "citation_handles": ["3", "4"]
            }))
            .is_ok()
        );
        assert!(
            parse_tool_sentex_handles(&json!({
                "claim_handles": ["1", "2"],
                "citation_handles": ["3", "4", "5"]
            }))
            .is_err()
        );
        let oversized_invalid = vec![Value::Null; MAX_SENTEX_REFS + 1];
        assert_eq!(
            parse_tool_sentex_handles(&json!({ "claim_handles": oversized_invalid }))
                .unwrap_err()
                .to_string(),
            "claim_handles and citation_handles support at most 4 total references"
        );
    }

    #[test]
    fn terminal_sentex_syntax_distinguishes_absent_from_invalid() {
        assert!(!has_terminal_sentex_syntax("ordinary prose"));
        assert!(!has_terminal_sentex_syntax(
            "quoted `[🔍=v2:claim:garbage]`"
        ));
        assert!(!has_terminal_sentex_syntax(
            "discussion [🔍=v2:claim:garbage] (see appendix [A])"
        ));
        assert!(has_terminal_sentex_syntax(
            "malformed [🔍=v2:claim:garbage]"
        ));
        assert!(has_terminal_sentex_syntax("current [🔍=v3:claim:CRY]"));
        assert!(has_terminal_sentex_syntax(
            "malformed [🔍=v3:citation:garbage]"
        ));
        assert!(has_terminal_sentex_syntax(concat!(
            "over limit ",
            "[🔍=v2:claim:AAAAAAAAAAE] ",
            "[🔍=v2:claim:AAAAAAAAAAI] ",
            "[🔍=v2:claim:AAAAAAAAAAM] ",
            "[🔍=v2:claim:AAAAAAAAAAQ] ",
            "[🔍=v2:claim:AAAAAAAAAAU]"
        )));
    }

    #[test]
    fn appending_markers_uses_role_preserving_v3_minimal_base64url() {
        let handles = parse_tool_sentex_handles(&json!({
            "claim_handles": ["12"],
            "citation_handles": ["34"]
        }))
        .unwrap();
        assert_eq!(
            append_markers("claim", &handles),
            "claim [🔍=v3:claim:DA] [🔍=v3:citation:Ig]"
        );
    }

    #[test]
    fn v3_tokens_are_minimal_width_and_round_trip_at_byte_boundaries() {
        for (handle, token) in [
            (1u64, "AQ"),
            (255, "_w"),
            (256, "AQA"),
            (2326, "CRY"),
            (65535, "__8"),
            (65536, "AQAA"),
            (0x00ff_ffff, "____"),
            (0x0100_0000, "AQAAAA"),
            (0xffff_ffff, "_____w"),
            (0x0001_0000_0000, "AQAAAAA"),
            (0x00ff_ffff_ffff, "______8"),
            (0x0100_0000_0000, "AQAAAAAA"),
            (0xffff_ffff_ffff, "________"),
            (0x0001_0000_0000_0000, "AQAAAAAAAA"),
            (0x00ff_ffff_ffff_ffff, "_________w"),
            (0x0100_0000_0000_0000, "AQAAAAAAAAA"),
            (0x0102_0304_0506_0708, "AQIDBAUGBwg"),
            (u64::MAX, "__________8"),
        ] {
            let handles =
                parse_tool_sentex_handles(&json!({ "citation_handles": [handle.to_string()] }))
                    .unwrap();
            let content = append_markers("claim", &handles);
            assert_eq!(content, format!("claim [🔍=v3:citation:{token}]"));
            let locators = parse_sentex_locators(&content);
            assert_eq!(locators.len(), 1, "{content}");
            assert_eq!(locators[0].as_str(), format!("v3:citation:{token}"));
            assert_eq!(locators[0].role(), SentexRole::Citation);
        }
    }

    #[test]
    fn v3_terminal_markers_parse_with_roles_and_order() {
        let content = "claim [🔍=v3:citation:CRY] [🔍=v3:claim:DA]";
        let sentexes = parse_offered_sentexes(content, UserId::new(99));
        assert_eq!(sentexes.len(), 2);
        assert_eq!(sentexes[0].locator.role(), SentexRole::Citation);
        assert_eq!(sentexes[0].locator.as_str(), "v3:citation:CRY");
        assert_eq!(sentexes[1].locator.role(), SentexRole::Claim);
        assert_eq!(sentexes[1].locator.as_str(), "v3:claim:DA");
    }

    #[test]
    fn v3_decoding_rejects_noncanonical_tokens() {
        for token in [
            // Empty.
            "",
            // Zero, as one byte and as the padded eight-byte form.
            "AA",
            "AAAAAAAAAAA",
            // Leading zero byte, including the v2-style padded form under v3.
            "AAkW",
            "AAAAAAAACRY",
            // Padding characters.
            "CRY=",
            "AQ==",
            // Standard (non-url-safe) alphabet and other non-base64url bytes.
            "CR+",
            "CR/",
            "CR Y",
            "CR.Y",
            // Nonzero unused trailing bits.
            "CRZ",
            "AR",
            // Impossible base64 length.
            "A",
            "AQIDB",
            // More than eight bytes.
            "AQAAAAAAAAAA",
        ] {
            let content = format!("claim [🔍=v3:claim:{token}]");
            assert!(
                parse_sentex_locators(&content).is_empty(),
                "accepted noncanonical v3 token {token:?}"
            );
        }
    }

    #[test]
    fn mixed_version_suffix_keeps_each_locator_as_written() {
        let content = concat!(
            "claim ",
            "[🔍=v1:AAAAAAAAACI] ",
            "[🔍=v2:claim:AAAAAAAAAAw] ",
            "[🔍=v3:citation:CRY]"
        );
        let locators = parse_sentex_locators(content);
        let rendered: Vec<_> = locators.iter().map(SentexLocator::as_str).collect();
        assert_eq!(
            rendered,
            ["v1:AAAAAAAAACI", "v2:claim:AAAAAAAAAAw", "v3:citation:CRY"]
        );
    }

    #[test]
    fn fixed_width_v1_and_v2_tokens_still_decode_to_the_same_handles() {
        assert_eq!(
            VaeliiSentexHandle::from_locator_token("AAAAAAAAAAw"),
            Some(VaeliiSentexHandle(NonZeroU64::new(12).unwrap()))
        );
        assert_eq!(
            VaeliiSentexHandle::from_locator_token("AAAAAAAACRY"),
            Some(VaeliiSentexHandle(NonZeroU64::new(2326).unwrap()))
        );
        // Minimal tokens are not valid in the fixed-width v1/v2 grammar.
        assert!(parse_sentex_locators("claim [🔍=v2:claim:CRY]").is_empty());
        assert!(parse_sentex_locators("claim [🔍=v1:CRY]").is_empty());
    }

    #[test]
    fn u64_max_string_round_trips_through_the_canonical_locator() {
        let handles =
            parse_tool_sentex_handles(&json!({ "claim_handles": [u64::MAX.to_string()] })).unwrap();
        let content = append_markers("claim", &handles);
        assert_eq!(content, "claim [🔍=v3:claim:__________8]");
        assert_eq!(
            parse_sentex_locators(&content)[0].as_str(),
            "v3:claim:__________8"
        );
    }

    #[test]
    fn locator_metadata_is_role_preserving_canonical_and_ordered() {
        let handles = parse_tool_sentex_handles(&json!({
            "claim_handles": ["12", "34"],
            "citation_handles": ["56"]
        }))
        .unwrap();
        assert_eq!(
            locator_metadata(handles.claims(), SentexRole::Claim),
            "v3:claim:DA,v3:claim:Ig"
        );
        assert_eq!(
            locator_metadata(handles.citations(), SentexRole::Citation),
            "v3:citation:OA"
        );
    }

    #[test]
    fn minimal_token_decoder_is_canonical() {
        let handle = |value| Some(VaeliiSentexHandle(NonZeroU64::new(value).unwrap()));
        assert_eq!(
            VaeliiSentexHandle::from_minimal_locator_token("CRY"),
            handle(2326)
        );
        assert_eq!(
            VaeliiSentexHandle::from_minimal_locator_token("AQ"),
            handle(1)
        );
        assert_eq!(
            VaeliiSentexHandle::from_minimal_locator_token("__________8"),
            handle(u64::MAX)
        );
        for token in [
            "",
            "AA",
            "AAkW",
            "AAAAAAAACRY",
            "CRY=",
            "CR+",
            "CRZ",
            "AQAAAAAAAAAA",
        ] {
            assert!(
                VaeliiSentexHandle::from_minimal_locator_token(token).is_none(),
                "accepted {token:?}"
            );
        }
    }

    #[test]
    fn minimal_decoder_rejects_tokens_longer_than_eleven_chars() {
        // Twelve characters decode to nine bytes, which do not fit in a u64.
        assert!(VaeliiSentexHandle::from_minimal_locator_token("AQAAAAAAAAAA").is_none());
        assert!(VaeliiSentexHandle::from_minimal_locator_token("____________").is_none());
    }

    #[test]
    fn minimal_decoder_rejects_the_empty_token() {
        assert!(VaeliiSentexHandle::from_minimal_locator_token("").is_none());
    }

    #[test]
    fn minimal_decoder_rejects_a_leading_zero_byte() {
        for token in ["AA", "AAE", "AAAAAAAACRY"] {
            assert!(
                VaeliiSentexHandle::from_minimal_locator_token(token).is_none(),
                "accepted {token:?}"
            );
        }
    }

    #[test]
    fn minimal_decoder_rejects_padding_and_nonzero_trailing_bits() {
        for token in ["CRY=", "AQ==", "CRZ", "AR"] {
            assert!(
                VaeliiSentexHandle::from_minimal_locator_token(token).is_none(),
                "accepted {token:?}"
            );
        }
    }

    #[test]
    fn marker_bytes_match_the_appended_suffix_exactly() {
        for (claims, citations) in [
            (vec!["1"], vec![]),
            (vec![], vec!["2326"]),
            (vec!["255", "256"], vec!["65536"]),
            (
                vec!["1", "65535"],
                vec!["18446744073709551615", "72623859790382856"],
            ),
        ] {
            let handles = parse_tool_sentex_handles(&json!({
                "claim_handles": claims,
                "citation_handles": citations
            }))
            .unwrap();
            let content = "claim";
            assert_eq!(
                append_markers(content, &handles).len() - content.len(),
                marker_bytes(&handles)
            );
        }
    }

    #[test]
    fn marker_byte_bound_covers_the_largest_allowed_suffix() {
        let largest = SentexHandles {
            claims: Vec::new(),
            citations: std::iter::repeat_with(|| {
                VaeliiSentexHandle(NonZeroU64::new(u64::MAX).unwrap())
            })
            .take(MAX_SENTEX_REFS)
            .collect(),
        };
        assert_eq!(marker_bytes(&largest), MAX_SENTEX_MARKER_BYTES);
        assert!(validate_marker_bytes(&largest).is_ok());
        // Minimal tokens make small handles cheap, so only full-width handles
        // beyond the count limit exceed the byte bound.
        let over_limit = SentexHandles {
            claims: Vec::new(),
            citations: std::iter::repeat_with(|| {
                VaeliiSentexHandle(NonZeroU64::new(u64::MAX).unwrap())
            })
            .take(MAX_SENTEX_REFS + 1)
            .collect(),
        };
        assert!(validate_marker_bytes(&over_limit).is_err());
    }

    #[test]
    fn marker_byte_bound_is_pinned_from_above() {
        let citations = |values: &[u64]| SentexHandles {
            claims: Vec::new(),
            citations: values
                .iter()
                .map(|value| VaeliiSentexHandle(NonZeroU64::new(*value).unwrap()))
                .collect(),
        };
        // Token lengths 11 + 4 + 3 + 3 + 3: exactly the limit, so accepted.
        let at_limit = citations(&[u64::MAX, 0x00ff_ffff, 256, 256, 256]);
        assert_eq!(marker_bytes(&at_limit), MAX_SENTEX_MARKER_BYTES);
        assert!(validate_marker_bytes(&at_limit).is_ok());
        // One token one character longer (3 -> 4): one byte over, so rejected.
        let over_by_one = citations(&[u64::MAX, 0x00ff_ffff, 0x00ff_ffff, 256, 256]);
        assert_eq!(marker_bytes(&over_by_one), MAX_SENTEX_MARKER_BYTES + 1);
        assert!(validate_marker_bytes(&over_by_one).is_err());
    }
}

#[cfg(test)]
mod error_tests {
    use super::*;
    use std::error::Error;
    #[test]
    fn overflowing_tool_handle_retains_numeric_cause_and_literal_message() {
        let error = parse_tool_sentex_handles(&json!({"claim_handles":["18446744073709551616"]}))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "claim_handles must contain canonical positive decimal u64 strings"
        );
        let SentexErrorKind::InvalidHandle { field, cause } = &error.kind else {
            panic!("invalid handle category");
        };
        assert_eq!(*field, SentexField::Claims);
        assert!(matches!(cause.kind, HandleErrorKind::Number(_)));
        assert!(
            cause
                .source()
                .unwrap()
                .source()
                .unwrap()
                .is::<std::num::ParseIntError>()
        );
        assert!(
            !parse_tool_sentex_handles(&json!({"claim_handles":["18446744073709551615"]}))
                .unwrap()
                .is_empty()
        );
    }
}
