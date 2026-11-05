use crate::Bell;

/// Render admitted bells into the compact wire format with full provenance.
///
/// Format: `"3s lain/person-pace@personal d=0.12 id=a rid=r_test;1l shared/foo@cc d=0 id=b rid=r_test2"`
/// - `{loudness}{timbre} {scope}/{name}@{provider_alias} d={distance} id={memory_id} rid={recall_id}`.
pub fn render_bells(bells: &[Bell]) -> String {
    bells
        .iter()
        .map(|bell| {
            format!(
                "{}{} {}/{}@{} d={} id={} rid={}",
                bell.loudness,
                bell.timbre.code(),
                bell.scope,
                bell.memory_name,
                bell.provider_alias,
                bell.distance,
                bell.memory_id,
                bell.recall_id,
            )
        })
        .collect::<Vec<_>>()
        .join(";")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BellTimbre;
    #[test]
    fn render_bells_produces_compact_format_with_provenance() {
        let bells = vec![
            Bell {
                provider_url: "http://memory/mcp".to_owned(),
                provider_alias: "personal".to_owned(),
                scope: "lain".to_owned(),
                recall_id: "r_test".to_owned(),
                memory_id: "a".to_owned(),
                memory_name: "person-pace".to_owned(),
                timbre: BellTimbre::Semantic,
                distance: 0.12,
                loudness: 3,
                provider_rank: 0,
            },
            Bell {
                provider_url: "http://cc/mcp".to_owned(),
                provider_alias: "cc".to_owned(),
                scope: "shared".to_owned(),
                recall_id: "r_test2".to_owned(),
                memory_id: "b".to_owned(),
                memory_name: "construct-cosmology".to_owned(),
                timbre: BellTimbre::Both,
                distance: 0.25,
                loudness: 2,
                provider_rank: 0,
            },
        ];
        assert_eq!(
            render_bells(&bells),
            "3s lain/person-pace@personal d=0.12 id=a rid=r_test;2b shared/construct-cosmology@cc d=0.25 id=b rid=r_test2"
        );
    }
    #[test]
    fn render_bells_preserves_distance_precision() {
        let bells = vec![Bell {
            loudness: 1,
            timbre: BellTimbre::Semantic,
            scope: "lain".to_owned(),
            memory_name: "test".to_owned(),
            provider_alias: "personal".to_owned(),
            provider_url: "http://localhost/mcp".to_owned(),
            provider_rank: 0,
            distance: 0.004,
            memory_id: "a".to_owned(),
            recall_id: "r_1".to_owned(),
        }];
        let rendered = render_bells(&bells);
        assert!(
            rendered.contains("d=0.004"),
            "distance 0.004 must not be rounded to 0.00, got: {rendered}"
        );
    }
}
