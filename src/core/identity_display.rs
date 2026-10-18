//! Literal human-facing identity display, independent of identity authority.

/// Format a supplied observed/cached name without fetching or escaping it.
pub(crate) fn format_identity(id: &str, display_name: Option<&str>) -> String {
    match display_name.filter(|name| !name.trim().is_empty()) {
        Some(name) => format!("{id}={name}"),
        None => id.to_owned(),
    }
}

/// Preserve explicit display-name pronouns over separately obtained enrichment.
pub(crate) fn display_name_with_pronouns(name: &str, pronouns: Option<&str>) -> String {
    match pronouns.filter(|value| !value.trim().is_empty()) {
        Some(value) if !has_pronoun_suffix(name) => format!("{name} ({value})"),
        _ => name.to_owned(),
    }
}

fn has_pronoun_suffix(name: &str) -> bool {
    let Some(end) = name.trim_end().strip_suffix(')') else {
        return false;
    };
    let Some((prefix, group)) = end.rsplit_once('(') else {
        return false;
    };
    let prefix_balance = prefix.bytes().try_fold(0_usize, |depth, byte| match byte {
        b'(' => depth.checked_add(1),
        b')' => depth.checked_sub(1),
        _ => Some(depth),
    });
    if prefix_balance != Some(0) || group.contains(['(', ')']) {
        return false;
    }
    let group = group.trim().to_ascii_lowercase();
    if matches!(
        group.as_str(),
        "any pronouns" | "other pronouns" | "ask me my pronouns" | "avoid pronouns, use my name"
    ) {
        return true;
    }
    group.split(" or ").all(|set| {
        let tokens: Vec<_> = set.split('/').map(str::trim).collect();
        (2..=5).contains(&tokens.len())
            && tokens.into_iter().all(|token| {
                matches!(
                    token,
                    "he" | "him"
                        | "his"
                        | "she"
                        | "her"
                        | "hers"
                        | "they"
                        | "them"
                        | "their"
                        | "theirs"
                        | "it"
                        | "its"
                        | "xe"
                        | "xem"
                        | "xyr"
                        | "xyrs"
                        | "ze"
                        | "zir"
                        | "zirs"
                        | "ey"
                        | "em"
                        | "eir"
                        | "eirs"
                        | "fae"
                        | "faer"
                        | "faers"
                )
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_display_uses_supplied_name_or_bare_identity_without_normalizing_text() {
        for (id, name, expected) in [
            (
                "123",
                Some("FixtureComet (she/her)"),
                "123=FixtureComet (she/her)",
            ),
            ("123", Some("  Iris  "), "123=  Iris  "),
            ("123", None, "123"),
            ("123", Some(" \t\n"), "123"),
            ("opaque/user", Some("Alex"), "opaque/user=Alex"),
        ] {
            assert_eq!(format_identity(id, name), expected);
        }
    }

    #[test]
    fn explicit_pronoun_suffix_takes_precedence_including_xe_it() {
        for name in [
            "Iris (xe/it)",
            "Iris (SHE/HER)",
            "Iris (they/them) ",
            "Iris (she/her or they/them)",
            "Iris (any pronouns)",
            "Iris (other pronouns)",
            "Iris (ask me my pronouns)",
            "Iris (avoid pronouns, use my name)",
        ] {
            assert_eq!(display_name_with_pronouns(name, Some("she/her")), name);
        }
    }

    #[test]
    fn unmatched_outer_group_is_not_an_explicit_pronoun_suffix() {
        for (name, expected) in [
            ("Alex ((she/her)", "Alex ((she/her) (they/them)"),
            ("Alex (team (xe/it)", "Alex (team (xe/it) (they/them)"),
            ("Alex (team) (she/her)", "Alex (team) (she/her)"),
        ] {
            assert_eq!(
                display_name_with_pronouns(name, Some("they/them")),
                expected
            );
        }
    }

    #[test]
    fn arbitrary_parentheses_do_not_suppress_available_pronouns() {
        for (name, expected) in [
            ("Alex (admin)", "Alex (admin) (she/her)"),
            ("Alex (team)", "Alex (team) (she/her)"),
            ("Alex (she/her fan)", "Alex (she/her fan) (she/her)"),
            ("Alex ((she/her))", "Alex ((she/her)) (she/her)"),
            ("Alex (xe/it", "Alex (xe/it (she/her)"),
            ("Alex (x/y)", "Alex (x/y) (she/her)"),
            ("Alex", "Alex (she/her)"),
        ] {
            assert_eq!(display_name_with_pronouns(name, Some("she/her")), expected);
        }
        assert_eq!(display_name_with_pronouns("Alex", None), "Alex");
        assert_eq!(display_name_with_pronouns("Alex", Some(" \t")), "Alex");
    }
}
