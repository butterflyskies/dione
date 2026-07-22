//! Typed pronoun identity metadata and provider boundary.

use serenity::model::id::UserId;
use std::{collections::HashSet, future::Future, pin::Pin};

/// One PronounDB set supported by the English v2 API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PronounSet {
    He,
    It,
    She,
    They,
    Any,
    Ask,
    Avoid,
    Other,
}

impl PronounSet {
    pub(crate) fn parse(code: &str) -> Option<Self> {
        Some(match code {
            "he" => Self::He,
            "it" => Self::It,
            "she" => Self::She,
            "they" => Self::They,
            "any" => Self::Any,
            "ask" => Self::Ask,
            "avoid" => Self::Avoid,
            "other" => Self::Other,
            _ => return None,
        })
    }

    fn display(self) -> &'static str {
        match self {
            Self::He => "he/him",
            Self::It => "it/its",
            Self::She => "she/her",
            Self::They => "they/them",
            Self::Any => "any pronouns",
            Self::Ask => "ask me my pronouns",
            Self::Avoid => "use my name",
            Self::Other => "other pronouns",
        }
    }

    fn nominative(self) -> Option<&'static str> {
        match self {
            Self::He => Some("he"),
            Self::It => Some("it"),
            Self::She => Some("she"),
            Self::They => Some("they"),
            Self::Any | Self::Ask | Self::Avoid | Self::Other => None,
        }
    }
}

/// Validated, nonempty English pronoun metadata for one opted-in user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PronounSelection(Vec<PronounSet>);

impl PronounSelection {
    pub(crate) fn from_codes(codes: &[String]) -> Option<Self> {
        if codes.is_empty() {
            return None;
        }
        let unique = codes.iter().map(String::as_str).collect::<HashSet<_>>();
        if unique.len() != codes.len() {
            return None;
        }
        let sets = codes
            .iter()
            .map(|code| PronounSet::parse(code))
            .collect::<Option<Vec<_>>>()?;
        Some(Self(sets))
    }

    pub fn display(&self) -> String {
        if self.0.len() > 1 && self.0.iter().all(|set| set.nominative().is_some()) {
            return self
                .0
                .iter()
                .filter_map(|set| set.nominative())
                .collect::<Vec<_>>()
                .join("/");
        }
        self.0
            .iter()
            .map(|set| set.display())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PronounProviderError {
    Transport,
    MalformedResponse,
}

/// Narrow provider seam; the Discord ingress layer owns deadline enforcement.
pub trait PronounProvider: Send + Sync {
    fn lookup(
        &self,
        user_id: UserId,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<PronounSelection>, PronounProviderError>> + Send + '_,
        >,
    >;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_special_sets_as_identity_metadata() {
        let codes = ["any", "ask", "avoid", "other"].map(String::from);
        let selection = PronounSelection::from_codes(&codes).expect("supported sets");
        assert_eq!(
            selection.display(),
            "any pronouns, ask me my pronouns, use my name, other pronouns"
        );
    }

    #[test]
    fn rejects_empty_or_unknown_sets() {
        assert_eq!(PronounSelection::from_codes(&[]), None);
        assert_eq!(PronounSelection::from_codes(&["unknown".to_owned()]), None);
    }

    #[test]
    fn rejects_duplicate_sets_and_accepts_the_complete_documented_set() {
        assert_eq!(
            PronounSelection::from_codes(&["she".to_owned(), "she".to_owned()]),
            None
        );
        let all_documented =
            ["he", "it", "she", "they", "any", "ask", "avoid", "other"].map(String::from);
        assert!(PronounSelection::from_codes(&all_documented).is_some());
    }

    #[test]
    fn renders_documented_v2_nominative_sets() {
        let single = PronounSelection::from_codes(&["she".to_owned()]).expect("supported set");
        assert_eq!(single.display(), "she/her");

        let combined = PronounSelection::from_codes(&["she".to_owned(), "it".to_owned()])
            .expect("supported sets");
        assert_eq!(combined.display(), "she/it");
    }
}
